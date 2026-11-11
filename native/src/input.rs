use crate::protocol::{InputEvent, MouseButton};
use rdev::{simulate, Button, EventType, Key};
#[cfg(not(target_os="macos"))] use rdev::{grab, Event};
#[cfg(not(target_os="macos"))] use std::sync::Mutex;
use std::{sync::{atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering}, mpsc::Sender, Arc}, time::Duration};

#[derive(Debug, Clone)]
pub enum Captured { EnterRemote, ReturnLocal, Event(InputEvent) }

/// rdev hands over absolute pointer positions, so while this machine drives the peer the hidden
/// local cursor is only a motion accumulator: a border that clamps it makes the delta zero and
/// freezes the peer cursor. Within this many points of a non-trigger border the ghost cursor is
/// pulled back inside so physical travel keeps producing deltas.
const REANCHOR_MARGIN: f64 = 40.0;
/// Travel room restored by one re-anchor, in points.
const REANCHOR_PULL: f64 = 320.0;

/// Wheel deltas travel as 1/120 of a notch, the unit Windows' WHEEL_DELTA and rdev's Windows
/// backend use. Each capture layer scales its own device unit up to this, each injector back down.
pub const WHEEL_TICKS_PER_NOTCH: i64 = 120;
/// macOS reports and injects scroll pixels, and a physical notch is about ten of them.
#[cfg(target_os="macos")]
pub const WHEEL_TICKS_PER_UNIT: i64 = WHEEL_TICKS_PER_NOTCH / 10;
/// rdev's Windows backend already reports and consumes whole notches.
#[cfg(not(target_os="macos"))]
pub const WHEEL_TICKS_PER_UNIT: i64 = WHEEL_TICKS_PER_NOTCH;

/// Which border hands control over: the peer screen sits on that side of this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Edge { #[default] Right, Left, Top, Bottom }
impl Edge {
    pub fn parse(s:&str)->Option<Self>{Some(match s{"right"=>Self::Right,"left"=>Self::Left,"top"=>Self::Top,"bottom"=>Self::Bottom,_=>return None})}
    pub fn as_str(self)->&'static str{match self{Self::Right=>"right",Self::Left=>"left",Self::Top=>"top",Self::Bottom=>"bottom"}}
    pub fn code(self)->u32{match self{Self::Right=>0,Self::Left=>1,Self::Top=>2,Self::Bottom=>3}}
    pub fn from_code(c:u32)->Self{match c{1=>Self::Left,2=>Self::Top,3=>Self::Bottom,_=>Self::Right}}
    /// True when the handoff band sits at the low border, so the hand-back band is the far one.
    pub fn low(self)->bool{matches!(self,Self::Left|Self::Top)}
    pub fn vertical(self)->bool{matches!(self,Self::Top|Self::Bottom)}
    /// The pointer coordinate this edge watches.
    pub fn coord(self,x:f64,y:f64)->f64{if self.vertical(){y}else{x}}
}

/// Union of every display in global coordinates. A monitor left of the primary makes x negative,
/// so geometry must not assume the desktop starts at zero or ends at the primary's width.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect { pub x: f64, pub y: f64, pub w: f64, pub h: f64 }
impl Rect {
    pub fn lo(self, vertical:bool)->f64{ if vertical {self.y} else {self.x} }
    pub fn hi(self, vertical:bool)->f64{ if vertical {self.y+self.h} else {self.x+self.w} }
}

#[derive(Clone)]
pub struct HookState {
    pub enabled: Arc<AtomicBool>,
    pub remote: Arc<AtomicBool>,
    pub receiving: Arc<AtomicBool>,
    pub injection: Arc<AtomicBool>,
    pub cursor_hidden: Arc<AtomicBool>,
    pub edge_code: Arc<AtomicU32>,
    pub screen_x: Arc<AtomicI32>,
    pub screen_y: Arc<AtomicI32>,
    pub screen_w: Arc<AtomicU32>,
    pub screen_h: Arc<AtomicU32>,
}
impl HookState {
    pub fn new() -> Self { Self { enabled:Arc::new(AtomicBool::new(false)), remote:Arc::new(AtomicBool::new(false)), receiving:Arc::new(AtomicBool::new(false)), injection:Arc::new(AtomicBool::new(false)), cursor_hidden:Arc::new(AtomicBool::new(false)), edge_code:Arc::new(AtomicU32::new(Edge::Right.code())), screen_x:Arc::new(AtomicI32::new(0)), screen_y:Arc::new(AtomicI32::new(0)), screen_w:Arc::new(AtomicU32::new(0)), screen_h:Arc::new(AtomicU32::new(0)) } }
    pub fn set_screen(&self,x:i32,y:i32,w:u32,h:u32){ self.screen_x.store(x,Ordering::Relaxed); self.screen_y.store(y,Ordering::Relaxed); self.screen_w.store(w,Ordering::Relaxed); self.screen_h.store(h,Ordering::Relaxed); }
    pub fn rect(&self)->Rect{ Rect{ x:self.screen_x.load(Ordering::Relaxed) as f64, y:self.screen_y.load(Ordering::Relaxed) as f64, w:self.screen_w.load(Ordering::Relaxed) as f64, h:self.screen_h.load(Ordering::Relaxed) as f64 } }
    pub fn edge(&self)->Edge{ Edge::from_code(self.edge_code.load(Ordering::Relaxed)) }
    pub fn set_edge(&self,e:Edge){ self.edge_code.store(e.code(),Ordering::Relaxed); }
    /// Where the hidden ghost cursor parks while this machine drives the peer.
    pub fn anchor(&self)->(f64,f64){ anchor_point(self.rect(), self.edge()) }
}

#[derive(Default)]
pub(crate) struct HookMemory { last:(f64,f64), injected:Option<(f64,f64)>, ctrl:bool, alt:bool }

/// One captured device event, in whatever form the platform's hook can supply.
/// `Move.relative` says whether `dx`/`dy` come from the device; when they do not, the position is
/// the only signal and the accumulator has to follow the real pointer.
pub enum Raw {
    Move { x: f64, y: f64, dx: f64, dy: f64, relative: bool },
    Key { code: Key, down: bool },
    Button { button: Button, down: bool },
    Wheel { dx: i64, dy: i64 },
    Other,
}

/// Pointer left the desktop through the handoff edge, so control should move to the peer. The
/// receiver uses the same test on its own peer-facing edge to hand control back.
pub fn entry_crossed(coord:f64, lo:f64, hi:f64, low:bool)->bool { hi-lo>2.0 && if low { coord<=lo+2.0 } else { coord>=hi-2.0 } }
/// Pointer reached the hand-back edge, the side opposite the handoff edge.
pub fn return_reached(coord:f64, lo:f64, hi:f64, low:bool)->bool { if low { coord>=hi-3.0 } else { coord<=lo+2.0 } }

/// Ghost cursor resting point: clear of both trigger bands, with travel room towards the handoff
/// edge and on both borders of the other axis.
pub fn anchor_point(r:Rect, edge:Edge)->(f64,f64){
    if r.w<2.0 || r.h<2.0 { return (r.x,r.y); }
    let room=REANCHOR_MARGIN+REANCHOR_PULL;
    let (cx,cy)=(r.x+r.w*0.5, r.y+r.h*0.5);
    match edge {
        Edge::Left => (r.x+room.min(r.w*0.5), cy),
        Edge::Right => ((r.hi(false)-1.0-room).max(cx), cy),
        Edge::Top => (cx, r.y+room.min(r.h*0.5)),
        Edge::Bottom => (cx, (r.hi(true)-1.0-room).max(cy)),
    }
}

/// The obstacle border the ghost cursor is pressed against, if any. The handoff and hand-back bands
/// are deliberately left alone: they are triggers, not obstacles.
pub fn reanchor(x:f64, y:f64, r:Rect, edge:Edge)->Option<(f64,f64)>{
    if r.w<2.0 || r.h<2.0 { return None; }
    let (ax,ay)=anchor_point(r,edge);
    let m=REANCHOR_MARGIN;
    let (xlo,xhi,ylo,yhi)=(r.x, r.hi(false)-1.0, r.y, r.hi(true)-1.0);
    // On the watched axis only the border the pointer came through blocks it; the opposite border is
    // the hand-back trigger and has to stay reachable. Both borders of the other axis block.
    let x_stuck=match edge{Edge::Left=>x<=xlo+m,Edge::Right=>x>=xhi-m,Edge::Top|Edge::Bottom=>x<=xlo+m||x>=xhi-m};
    let y_stuck=match edge{Edge::Top=>y<=ylo+m,Edge::Bottom=>y>=yhi-m,Edge::Left|Edge::Right=>y<=ylo+m||y>=yhi-m};
    if !x_stuck && !y_stuck { return None; }
    Some((if x_stuck {ax} else {x}, if y_stuck {ay} else {y}))
}

/// Peer cursor position after one forwarded move, clamped to the peer desktop so it can come to rest
/// on the hand-back edge and release control from there.
pub fn clamp_move(current:f64, delta:f64, lo:f64, hi:f64)->f64{ if hi-lo<1.0 {lo} else {(current+delta).clamp(lo,hi-1.0)} }

/// Maps a desktop coordinate onto SendInput's 0..65535 absolute range using the same union rect the
/// handoff geometry uses. rdev divides by GetSystemMetrics(SM_CXVIRTUALSCREEN) instead, which is
/// DPI-virtualised and disagrees with the DEVMODE union on a scaled Windows desktop.
#[cfg_attr(not(target_os="windows"), allow(dead_code))]
pub fn absolute_units(coord:f64, origin:f64, span:f64)->i32{
    if span<1.0 {return 0;}
    (((coord-origin)/(span-1.0))*65535.0).round().clamp(0.0,65535.0) as i32
}

/// Decides what happens to one captured event. `true` lets it reach the local machine, `false`
/// swallows it. Both hook backends funnel into this so the handoff rules stay in one place.
pub(crate) fn handle(raw: Raw, state: &HookState, mem: &mut HookMemory, tx: &Sender<Captured>) -> bool {
    if !state.enabled.load(Ordering::Relaxed) { return true; }
    if let Raw::Key{code:Key::Escape, down:true} = raw {
        if mem.ctrl && mem.alt && (state.remote.swap(false,Ordering::SeqCst)||state.receiving.swap(false,Ordering::SeqCst)) {
            if mem.ctrl{let _=tx.send(Captured::Event(InputEvent::Key{code:key_name(Key::ControlLeft),down:false}));}
            if mem.alt{let _=tx.send(Captured::Event(InputEvent::Key{code:key_name(Key::Alt),down:false}));}
            restore_pointer(state,mem.last.0,mem.last.1);let _=tx.send(Captured::ReturnLocal);return false;
        }
    }
    match raw {
        Raw::Key{code:Key::ControlLeft|Key::ControlRight, down} => mem.ctrl=down,
        Raw::Key{code:Key::Alt|Key::AltGr, down} => mem.alt=down,
        Raw::Move{x,y,dx,dy,relative} => {
            let edge=state.edge();
            let r=state.rect();
            let (lo,hi)=(r.lo(edge.vertical()),r.hi(edge.vertical()));
            let low=edge.low();
            if state.injection.load(Ordering::Relaxed) {
                if state.receiving.load(Ordering::Relaxed) {
                    // Control returns at the border facing the driver, and only when an injected move
                    // pushes the cursor there. One already resting on that border must not bounce
                    // control back on the next unrelated move, or the wheel would stay on this machine.
                    let arrived=mem.injected.is_some_and(|p|!entry_crossed(edge.coord(p.0,p.1),lo,hi,low));
                    if arrived && entry_crossed(edge.coord(x,y),lo,hi,low) { state.receiving.store(false,Ordering::SeqCst); let _=tx.send(Captured::ReturnLocal); }
                    mem.injected=Some((x,y));
                } else {
                    // A local warp is not the peer's cursor, so it must not seed the next session.
                    mem.injected=None;
                }
                return true;
            }
            if state.receiving.load(Ordering::Relaxed) { return false; }
            let remote=state.remote.load(Ordering::Relaxed);
            // While driving the peer the position is a virtual accumulator fed by device deltas, so
            // a border that clamps the real cursor cannot stall the peer cursor.
            let (x,y)=if remote && relative {(mem.last.0+dx,mem.last.1+dy)} else {(x,y)};
            if !remote && entry_crossed(edge.coord(x,y),lo,hi,low) {
                state.remote.store(true,Ordering::SeqCst);
                let (ax,ay)=state.anchor();
                hide_pointer(state);mem.last=(ax,ay);warp_pointer(state,ax,ay);
                let _=tx.send(Captured::EnterRemote);return false;
            }
            if remote {
                if return_reached(edge.coord(x,y),lo,hi,low) { state.remote.store(false,Ordering::SeqCst);restore_pointer(state,x,y);let _=tx.send(Captured::ReturnLocal);return false; }
                let fdx=x-mem.last.0;let fdy=y-mem.last.1;
                if fdx!=0.0||fdy!=0.0 {let _=tx.send(Captured::Event(InputEvent::Move{dx:fdx,dy:fdy}));}
                mem.last = match reanchor(x,y,r,edge) {
                    Some(target) => { if !relative { warp_pointer(state,target.0,target.1); } target }
                    None => (x,y),
                };
                return true;
            }
            mem.last=(x,y);
        }
        _ if state.injection.load(Ordering::Relaxed) => return true,
        Raw::Key{..} | Raw::Button{..} | Raw::Wheel{..} if state.receiving.load(Ordering::Relaxed) => return false,
        Raw::Key{code,down} if state.remote.load(Ordering::Relaxed)=>{let _=tx.send(Captured::Event(InputEvent::Key{code:key_name(code),down}));return false;}
        Raw::Button{button,down} if state.remote.load(Ordering::Relaxed)=>{if let Some(b)=to_wire_button(button){let _=tx.send(Captured::Event(InputEvent::Button{button:b,down}));}return false;}
        Raw::Wheel{dx,dy} if state.remote.load(Ordering::Relaxed)=>{let _=tx.send(Captured::Event(InputEvent::Wheel{dx,dy}));return false;}
        _=>{}
    }
    true
}

/// Runs a suppressing system-wide hook. Call on a dedicated thread; macOS asks the
/// user to grant Accessibility and Input Monitoring to the app.
#[cfg(target_os="macos")]
pub fn start_hook(tx: Sender<Captured>, state: HookState) -> Result<(), String> {
    crate::tap::start(tx, state)
}

/// Windows low-level hook. rdev's conversion is main-thread-safe there, unlike its macOS tap.
#[cfg(not(target_os="macos"))]
pub fn start_hook(tx: Sender<Captured>, state: HookState) -> Result<(), String> {
    let tx=Arc::new(tx); let memory=Arc::new(Mutex::new(HookMemory::default()));
    grab(move |event:Event| {
        let raw=match event.event_type {
            EventType::MouseMove{x,y} => Raw::Move{x,y,dx:0.0,dy:0.0,relative:false},
            EventType::KeyPress(k) => Raw::Key{code:k,down:true},
            EventType::KeyRelease(k) => Raw::Key{code:k,down:false},
            EventType::ButtonPress(b) => Raw::Button{button:b,down:true},
            EventType::ButtonRelease(b) => Raw::Button{button:b,down:false},
            EventType::Wheel{delta_x,delta_y} => Raw::Wheel{dx:delta_x*WHEEL_TICKS_PER_UNIT,dy:delta_y*WHEEL_TICKS_PER_UNIT},
            _ => Raw::Other,
        };
        let Ok(mut mem)=memory.lock() else { return Some(event) };
        if handle(raw,&state,&mut mem,&tx) { Some(event) } else { None }
    }).map_err(|e|format!("global input hook failed: {e:?}"))
}

fn to_wire_button(b:Button)->Option<MouseButton>{match b{Button::Left=>Some(MouseButton::Left),Button::Right=>Some(MouseButton::Right),Button::Middle=>Some(MouseButton::Middle),_=>None}}

/// Receiver-side bookkeeping: where the injected cursor sits on the local desktop, and the wheel
/// fraction left over from the last injection.
#[derive(Clone, Copy, Debug)]
pub struct InjectState { pub cursor:(f64,f64), wheel:(f64,f64) }
impl InjectState {
    pub fn new(center:(f64,f64))->Self{ Self{cursor:center,wheel:(0.0,0.0)} }
}

/// Scales wire ticks into the platform's wheel unit, carrying the fraction: truncating each small
/// trackpad delta on its own would add up to no scrolling at all.
fn wheel_units(ticks:i64, carry:&mut f64)->i64{
    *carry += ticks as f64 / WHEEL_TICKS_PER_UNIT as f64;
    let whole=carry.trunc();
    *carry -= whole;
    whole as i64
}

/// Marks the whole injection as ours so the hook ignores the resulting event, and keeps the flag up
/// long enough for the event to come back through the hook.
fn guarded<T>(state:&HookState, post:impl FnOnce()->T)->T{
    state.injection.store(true,Ordering::SeqCst);
    let out=post();
    std::thread::sleep(Duration::from_millis(3));
    state.injection.store(false,Ordering::SeqCst);
    out
}

pub fn inject(event:&InputEvent, inj:&mut InjectState, state:&HookState)->Result<(),String>{
    let r=state.rect();
    let native=match event {
        InputEvent::Move{dx,dy}=>{
            inj.cursor.0=clamp_move(inj.cursor.0,*dx,r.x,r.hi(false));
            inj.cursor.1=clamp_move(inj.cursor.1,*dy,r.y,r.hi(true));
            #[cfg(target_os="windows")]
            return guarded(state,||platform_move(inj.cursor,r));
            #[cfg(not(target_os="windows"))]
            EventType::MouseMove{x:inj.cursor.0,y:inj.cursor.1}
        },
        InputEvent::Button{button,down}=>{let b=match button{MouseButton::Left=>Button::Left,MouseButton::Right=>Button::Right,MouseButton::Middle=>Button::Middle};if *down{EventType::ButtonPress(b)}else{EventType::ButtonRelease(b)}},
        InputEvent::Wheel{dx,dy}=>EventType::Wheel{delta_x:wheel_units(*dx,&mut inj.wheel.0),delta_y:wheel_units(*dy,&mut inj.wheel.1)},
        InputEvent::Key{code,down}=>{let Some(k)=parse_key(code) else{return Ok(())};if *down{EventType::KeyPress(k)}else{EventType::KeyRelease(k)}},
    };
    guarded(state,||simulate(&native).map_err(|e|format!("input injection failed: {e:?}")))
}

/// Key names on the wire are rdev's own variant spellings; `parse_key` is the accepting half.
fn key_name(k:Key)->String{format!("{k:?}")}

fn parse_key(s:&str)->Option<Key>{Some(match s{
"Escape"=>Key::Escape,"Return"=>Key::Return,"Space"=>Key::Space,"Tab"=>Key::Tab,"Backspace"=>Key::Backspace,"Delete"=>Key::Delete,"ShiftLeft"=>Key::ShiftLeft,"ShiftRight"=>Key::ShiftRight,"ControlLeft"=>Key::ControlLeft,"ControlRight"=>Key::ControlRight,"Alt"=>Key::Alt,"AltGr"=>Key::AltGr,"MetaLeft"=>Key::MetaLeft,"MetaRight"=>Key::MetaRight,"LeftArrow"=>Key::LeftArrow,"RightArrow"=>Key::RightArrow,"UpArrow"=>Key::UpArrow,"DownArrow"=>Key::DownArrow,
"KeyA"=>Key::KeyA,"KeyB"=>Key::KeyB,"KeyC"=>Key::KeyC,"KeyD"=>Key::KeyD,"KeyE"=>Key::KeyE,"KeyF"=>Key::KeyF,"KeyG"=>Key::KeyG,"KeyH"=>Key::KeyH,"KeyI"=>Key::KeyI,"KeyJ"=>Key::KeyJ,"KeyK"=>Key::KeyK,"KeyL"=>Key::KeyL,"KeyM"=>Key::KeyM,"KeyN"=>Key::KeyN,"KeyO"=>Key::KeyO,"KeyP"=>Key::KeyP,"KeyQ"=>Key::KeyQ,"KeyR"=>Key::KeyR,"KeyS"=>Key::KeyS,"KeyT"=>Key::KeyT,"KeyU"=>Key::KeyU,"KeyV"=>Key::KeyV,"KeyW"=>Key::KeyW,"KeyX"=>Key::KeyX,"KeyY"=>Key::KeyY,"KeyZ"=>Key::KeyZ,"Num0"=>Key::Num0,"Num1"=>Key::Num1,"Num2"=>Key::Num2,"Num3"=>Key::Num3,"Num4"=>Key::Num4,"Num5"=>Key::Num5,"Num6"=>Key::Num6,"Num7"=>Key::Num7,"Num8"=>Key::Num8,"Num9"=>Key::Num9,"Minus"=>Key::Minus,"Equal"=>Key::Equal,"Comma"=>Key::Comma,"Dot"=>Key::Dot,"Slash"=>Key::Slash,"SemiColon"=>Key::SemiColon,"Quote"=>Key::Quote,"BackQuote"=>Key::BackQuote,"LeftBracket"=>Key::LeftBracket,"RightBracket"=>Key::RightBracket,"BackSlash"=>Key::BackSlash,"Home"=>Key::Home,"End"=>Key::End,"PageUp"=>Key::PageUp,"PageDown"=>Key::PageDown,"F1"=>Key::F1,"F2"=>Key::F2,"F3"=>Key::F3,"F4"=>Key::F4,"F5"=>Key::F5,"F6"=>Key::F6,"F7"=>Key::F7,"F8"=>Key::F8,"F9"=>Key::F9,"F10"=>Key::F10,"F11"=>Key::F11,"F12"=>Key::F12,_=>return None})}

pub fn release_key(code:&str)->Result<(),String>{if let Some(k)=parse_key(code){simulate(&EventType::KeyRelease(k)).map_err(|e|format!("release key failed: {e:?}"))?;}Ok(())}
pub fn release_button(button:MouseButton)->Result<(),String>{let b=match button{MouseButton::Left=>Button::Left,MouseButton::Right=>Button::Right,MouseButton::Middle=>Button::Middle};simulate(&EventType::ButtonRelease(b)).map_err(|e|format!("release mouse button failed: {e:?}"))}

fn hide_pointer(state:&HookState){if !state.cursor_hidden.swap(true,Ordering::SeqCst){platform_hide();}}
pub fn restore_pointer(state:&HookState,x:f64,y:f64){if state.cursor_hidden.swap(false,Ordering::SeqCst){platform_show();let edge=state.edge();let r=state.rect();let v=edge.vertical();let inside=if edge.low(){r.lo(v)+3.0}else{r.hi(v)-3.0};let (rx,ry)=if v{(x.clamp(r.x,r.hi(false)-1.0),inside)}else{(inside,y.clamp(r.y,r.hi(true)-1.0))};warp_pointer(state,rx,ry);}}
fn warp_pointer(state:&HookState,x:f64,y:f64){guarded(state,||platform_warp(x,y,state.rect()));}
#[cfg(target_os="macos")]
fn platform_warp(x:f64,y:f64,_r:Rect){#[repr(C)]struct Point{x:f64,y:f64}#[link(name="CoreGraphics",kind="framework")]unsafe extern "C"{fn CGWarpMouseCursorPosition(p:Point)->i32;}unsafe{let _=CGWarpMouseCursorPosition(Point{x,y});}}
/// The cursor hide counter is per display: hiding only the main one leaves the ghost cursor visible
/// on every other screen of a multi-monitor desktop.
#[cfg(target_os="macos")]
fn display_ids()->Vec<u32>{#[link(name="CoreGraphics",kind="framework")]unsafe extern "C"{fn CGGetActiveDisplayList(max:u32,ids:*mut u32,count:*mut u32)->i32;fn CGMainDisplayID()->u32;}let mut ids=[0u32;16];let mut count=0u32;if unsafe{CGGetActiveDisplayList(ids.len() as u32,ids.as_mut_ptr(),&mut count)}!=0||count==0{return vec![unsafe{CGMainDisplayID()}];}ids[..count as usize].to_vec()}
#[cfg(target_os="macos")]
fn platform_hide(){#[link(name="CoreGraphics",kind="framework")]unsafe extern "C"{fn CGDisplayHideCursor(id:u32)->i32;}for id in display_ids(){unsafe{let _=CGDisplayHideCursor(id);}}}
#[cfg(target_os="macos")]
fn platform_show(){#[link(name="CoreGraphics",kind="framework")]unsafe extern "C"{fn CGDisplayShowCursor(id:u32)->i32;}for id in display_ids(){unsafe{let _=CGDisplayShowCursor(id);}}}
#[cfg(target_os="windows")]
fn platform_warp(x:f64,y:f64,r:Rect){let _=platform_move((x,y),r);}
/// Only mouse input is ever posted, so the INPUT union can be spelled as its largest member.
#[cfg(target_os="windows")]
#[repr(C)]
struct MouseInput{dx:i32,dy:i32,data:i32,flags:u32,time:u32,extra:usize}
#[cfg(target_os="windows")]
#[repr(C)]
struct Input{kind:u32,mouse:MouseInput}
#[cfg(target_os="windows")]
fn platform_move(cursor:(f64,f64),r:Rect)->Result<(),String>{
    #[link(name="user32")]unsafe extern "system"{fn SendInput(count:u32,inputs:*mut Input,size:i32)->u32;}
    const MOVE_ABSOLUTE_VIRTUALDESK:u32=0x0001|0x8000|0x4000;
    let mut input=Input{kind:0,mouse:MouseInput{
        dx:absolute_units(cursor.0,r.x,r.w),
        dy:absolute_units(cursor.1,r.y,r.h),
        data:0,flags:MOVE_ABSOLUTE_VIRTUALDESK,time:0,extra:0}};
    let sent=unsafe{SendInput(1,&mut input,std::mem::size_of::<Input>() as i32)};
    if sent==1{Ok(())}else{Err("SendInput rejected the absolute move".into())}
}
#[cfg(target_os="windows")]
fn platform_hide(){#[link(name="user32")]unsafe extern "system"{fn ShowCursor(show:i32)->i32;}unsafe{let _=ShowCursor(0);}}
#[cfg(target_os="windows")]
fn platform_show(){#[link(name="user32")]unsafe extern "system"{fn ShowCursor(show:i32)->i32;}unsafe{let _=ShowCursor(1);}}
#[cfg(not(any(target_os="macos",target_os="windows")))]
fn platform_warp(_:f64,_:f64,_:Rect){}
#[cfg(not(any(target_os="macos",target_os="windows")))]
fn platform_hide(){}
#[cfg(not(any(target_os="macos",target_os="windows")))]
fn platform_show(){}

#[cfg(test)]
mod tests {
    use super::*;

    const MAPPED:[Key;66]=[Key::Escape,Key::Return,Key::Space,Key::Tab,Key::Backspace,Key::Delete,Key::ShiftLeft,Key::ShiftRight,Key::ControlLeft,Key::ControlRight,Key::Alt,Key::AltGr,Key::MetaLeft,Key::MetaRight,Key::LeftArrow,Key::RightArrow,Key::UpArrow,Key::DownArrow,Key::KeyA,Key::KeyB,Key::KeyC,Key::KeyD,Key::KeyE,Key::KeyF,Key::KeyG,Key::KeyH,Key::KeyI,Key::KeyJ,Key::KeyK,Key::KeyL,Key::KeyM,Key::KeyN,Key::KeyO,Key::KeyP,Key::KeyQ,Key::KeyR,Key::KeyS,Key::KeyT,Key::KeyU,Key::KeyV,Key::KeyW,Key::KeyX,Key::KeyY,Key::KeyZ,Key::Num0,Key::Num1,Key::Num2,Key::Num3,Key::Num4,Key::Num5,Key::Num6,Key::Num7,Key::Num8,Key::Num9,Key::Minus,Key::Equal,Key::Comma,Key::Dot,Key::Slash,Key::SemiColon,Key::Quote,Key::BackQuote,Key::LeftBracket,Key::RightBracket,Key::BackSlash,Key::Home];

    #[test]
    fn forwarded_key_names_are_accepted_by_the_peer() {
        for k in MAPPED {
            let name=key_name(k);
            assert_eq!(parse_key(&name),Some(k),"{name} would be dropped in transit");
        }
    }

    #[test]
    fn escape_hatch_literals_match_the_wire_spelling() {
        assert_eq!(key_name(Key::ControlLeft),"ControlLeft");
        assert_eq!(key_name(Key::Alt),"Alt");
    }

    #[test]
    fn unmapped_keys_are_dropped_on_arrival() {
        assert_eq!(parse_key(&key_name(Key::CapsLock)),None);
        assert_eq!(parse_key(""),None);
        assert_eq!(parse_key("KeyAB"),None);
    }

    const SCREEN: Rect = Rect { x:0.0, y:0.0, w:1920.0, h:1080.0 };
    /// A second display to the left of and above the primary: the desktop origin is negative.
    const DUAL: Rect = Rect { x:-1440.0, y:-900.0, w:3360.0, h:1980.0 };

    fn bands(r:Rect, edge:Edge)->(f64,f64,bool){ let v=edge.vertical(); (r.lo(v),r.hi(v),edge.low()) }
    fn hand_back(r:Rect, edge:Edge, x:f64, y:f64)->bool{ let (lo,hi,low)=bands(r,edge); return_reached(edge.coord(x,y),lo,hi,low) }
    fn peer_facing(r:Rect, edge:Edge, x:f64, y:f64)->bool{ let (lo,hi,low)=bands(r,edge); entry_crossed(edge.coord(x,y),lo,hi,low) }

    /// Neither trigger band may catch the pointer on its own, or the ghost cursor would hand control
    /// over or back the moment it parks.
    fn assert_out_of_both_bands(r:Rect, edge:Edge, x:f64, y:f64, what:&str){
        assert!(!peer_facing(r,edge,x,y),"{what} would enter the peer");
        assert!(!hand_back(r,edge,x,y),"{what} would hand back");
    }

    /// A live hook state, without arming a real hook: nothing here may hide or warp the pointer.
    fn harness(edge:Edge, r:Rect)->(HookState,HookMemory,Sender<Captured>,std::sync::mpsc::Receiver<Captured>){
        let state=HookState::new();
        state.set_screen(r.x as i32,r.y as i32,r.w as u32,r.h as u32);
        state.set_edge(edge);
        state.enabled.store(true,Ordering::SeqCst);
        let (tx,rx)=std::sync::mpsc::channel();
        (state,HookMemory::default(),tx,rx)
    }

    #[test]
    fn handoff_and_handback_are_opposite_borders() {
        for r in [SCREEN,DUAL,Rect{x:0.0,y:0.0,w:800.0,h:600.0}] {
            for edge in [Edge::Right,Edge::Left,Edge::Top,Edge::Bottom] {
                let (lo,hi,low)=bands(r,edge);
                assert!(!entry_crossed((lo+hi)*0.5,lo,hi,low),"{edge:?} mid-desktop");
                let (handoff,other)=if low {(lo,hi-1.0)} else {(hi-1.0,lo)};
                assert!(entry_crossed(handoff,lo,hi,low),"{r:?} {edge:?}");
                assert!(return_reached(other,lo,hi,low),"{r:?} {edge:?}");
                assert!(!return_reached(handoff,lo,hi,low),"the handoff border must not also hand back");
            }
        }
    }

    #[test]
    fn a_degenerate_desktop_disables_handoff() {
        assert!(!entry_crossed(0.0,0.0,0.0,true));
        assert!(!entry_crossed(1919.0,0.0,0.0,false));
        assert!(!entry_crossed(-1920.0,-1920.0,-1919.0,true));
    }

    #[test]
    fn the_seam_between_two_displays_is_not_a_handoff_edge() {
        // Two 1920x1080 screens side by side: only x=3838 may hand over, never the seam at 1920.
        let wide=Rect{x:0.0,y:0.0,w:3840.0,h:1080.0};
        let (lo,hi,low)=bands(wide,Edge::Right);
        assert!(!entry_crossed(1919.0,lo,hi,low));
        assert!(!entry_crossed(1920.0,lo,hi,low));
        assert!(entry_crossed(3839.0,lo,hi,low));
        // The same desktop shifted so the secondary sits left of the primary.
        let (lo,hi,low)=bands(DUAL,Edge::Left);
        assert!(!entry_crossed(0.0,lo,hi,low));
        assert!(entry_crossed(-1439.0,lo,hi,low));
    }

    #[test]
    fn trigger_bands_are_not_re_anchored() {
        assert_eq!(reanchor(6.0,540.0,SCREEN,Edge::Right),None,"a flick left has to stay reachable");
        assert_eq!(reanchor(1913.0,540.0,SCREEN,Edge::Left),None,"a flick right has to stay reachable");
        assert_eq!(reanchor(960.0,1073.0,SCREEN,Edge::Top),None,"a flick down has to stay reachable");
        assert_eq!(reanchor(960.0,7.0,SCREEN,Edge::Bottom),None,"a flick up has to stay reachable");
        assert_eq!(reanchor(960.0,540.0,SCREEN,Edge::Right),None);
    }

    #[test]
    fn obstacle_borders_are_re_anchored_with_room() {
        let (w,h)=(SCREEN.w,SCREEN.h);
        assert_eq!(reanchor(w-1.0,540.0,SCREEN,Edge::Right),Some((1559.0,540.0)));
        assert_eq!(reanchor(3.0,540.0,SCREEN,Edge::Left),Some((360.0,540.0)));
        assert_eq!(reanchor(960.0,3.0,SCREEN,Edge::Right),Some((960.0,540.0)));
        assert_eq!(reanchor(960.0,h-1.0,SCREEN,Edge::Right),Some((960.0,540.0)));
        assert_eq!(reanchor(w-1.0,3.0,SCREEN,Edge::Right),Some((1559.0,540.0)));
        assert_eq!(reanchor(960.0,3.0,SCREEN,Edge::Top),Some((960.0,360.0)),"the border it came through pulls along the watched axis");
        assert_eq!(reanchor(960.0,h-1.0,SCREEN,Edge::Top),None,"the far border hands back instead of re-anchoring");
        assert_eq!(reanchor(3.0,540.0,SCREEN,Edge::Top),Some((960.0,540.0)),"the other axis pulls to its middle");
    }

    #[test]
    fn anchors_and_targets_stay_out_of_every_trigger_band() {
        for r in [SCREEN,DUAL,Rect{x:0.0,y:0.0,w:1024.0,h:768.0},Rect{x:0.0,y:0.0,w:500.0,h:400.0}] {
            for edge in [Edge::Right,Edge::Left,Edge::Top,Edge::Bottom] {
                let (ax,ay)=anchor_point(r,edge);
                assert!(ax>r.x+2.0 && ax<r.hi(false)-2.0 && ay>r.y+2.0 && ay<r.hi(true)-2.0,"{r:?} {edge:?} parked at ({ax},{ay})");
                assert_out_of_both_bands(r,edge,ax,ay,"the anchor");
                for (x,y) in [(r.x+1.0,r.y+r.h*0.5),(r.hi(false)-1.0,r.y+r.h*0.5),(r.x+r.w*0.5,r.y+1.0),(r.x+r.w*0.5,r.hi(true)-1.0)] {
                    if hand_back(r,edge,x,y) {
                        assert_eq!(reanchor(x,y,r,edge),None,"{edge:?} must keep its hand-back border a pure trigger");
                    } else {
                        let t=reanchor(x,y,r,edge).expect("obstacle border must re-anchor");
                        assert_out_of_both_bands(r,edge,t.0,t.1,"the re-anchor target");
                    }
                }
            }
        }
    }

    #[test]
    fn degenerate_screens_are_ignored() {
        assert_eq!(reanchor(0.0,0.0,Rect::default(),Edge::Right),None);
        assert_eq!(anchor_point(Rect::default(),Edge::Top),(0.0,0.0));
        assert_eq!(anchor_point(Rect{x:-5.0,y:-7.0,w:0.0,h:0.0},Edge::Top),(-5.0,-7.0));
    }

    #[test]
    fn peer_cursor_clamps_at_the_desktop_border() {
        assert_eq!(clamp_move(1919.0,5000.0,0.0,1920.0),1919.0);
        assert_eq!(clamp_move(1919.0,-3000.0,0.0,1920.0),0.0);
        assert_eq!(clamp_move(500.0,120.0,0.0,1920.0),620.0);
        assert_eq!(clamp_move(500.0,10.0,0.0,0.0),0.0);
        assert_eq!(clamp_move(-1440.0,-5000.0,DUAL.x,DUAL.hi(false)),-1440.0);
        assert_eq!(clamp_move(0.0,5000.0,DUAL.x,DUAL.hi(false)),1919.0);
        assert_eq!(clamp_move(0.0,-5000.0,DUAL.y,DUAL.hi(true)),-900.0);
    }

    #[test]
    fn absolute_moves_span_the_union_rect_not_the_primary_display() {
        assert_eq!(absolute_units(SCREEN.x,SCREEN.x,SCREEN.w),0);
        assert_eq!(absolute_units(SCREEN.hi(false)-1.0,SCREEN.x,SCREEN.w),65535);
        assert_eq!(absolute_units(DUAL.x,DUAL.x,DUAL.w),0);
        assert_eq!(absolute_units(DUAL.hi(false)-1.0,DUAL.x,DUAL.w),65535);
        assert_eq!(absolute_units(DUAL.y,DUAL.y,DUAL.h),0);
        assert_eq!(absolute_units(DUAL.hi(true)-1.0,DUAL.y,DUAL.h),65535);
        // A secondary display left of the primary one must still land inside the range.
        assert!(absolute_units(-1440.0,DUAL.x,DUAL.w)<absolute_units(0.0,DUAL.x,DUAL.w));
        assert_eq!(absolute_units(-99999.0,DUAL.x,DUAL.w),0);
        assert_eq!(absolute_units(99999.0,DUAL.x,DUAL.w),65535);
        assert_eq!(absolute_units(500.0,0.0,0.0),0);
    }

    #[test]
    fn the_hook_geometry_follows_the_whole_desktop() {
        let (state,_,_,_)=harness(Edge::Right,DUAL);
        assert_eq!(state.rect(),DUAL);
        assert_eq!(state.anchor(),(1559.0,90.0));
        state.set_edge(Edge::Left);
        assert_eq!(state.anchor(),(-1080.0,90.0));
    }

    #[test]
    fn the_wheel_follows_control_instead_of_staying_local() {
        let (state,mut mem,tx,rx)=harness(Edge::Right,SCREEN);
        assert!(handle(Raw::Wheel{dx:0,dy:WHEEL_TICKS_PER_NOTCH},&state,&mut mem,&tx),"a local wheel must scroll here");
        assert!(rx.try_recv().is_err());
        state.remote.store(true,Ordering::SeqCst);
        assert!(!handle(Raw::Wheel{dx:0,dy:-WHEEL_TICKS_PER_NOTCH},&state,&mut mem,&tx),"a forwarded wheel must not also scroll here");
        assert!(matches!(rx.try_recv(),Ok(Captured::Event(InputEvent::Wheel{dx:0,dy:-120}))));
        state.remote.store(false,Ordering::SeqCst);
        state.receiving.store(true,Ordering::SeqCst);
        assert!(!handle(Raw::Wheel{dx:0,dy:WHEEL_TICKS_PER_NOTCH},&state,&mut mem,&tx),"while being driven the local wheel is dropped");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn wheel_ticks_convert_without_losing_small_deltas() {
        let mut carry=0.0;
        assert_eq!(wheel_units(WHEEL_TICKS_PER_NOTCH,&mut carry),WHEEL_TICKS_PER_NOTCH/WHEEL_TICKS_PER_UNIT);
        assert_eq!(carry,0.0);
        let mut carry=0.0;
        let mut total=0;
        for _ in 0..WHEEL_TICKS_PER_UNIT { total+=wheel_units(1,&mut carry); }
        assert_eq!(total,1,"{WHEEL_TICKS_PER_UNIT} single-tick events must add up to one whole unit");
        let mut carry=0.0;
        assert_eq!(wheel_units(1,&mut carry),0);
        assert_eq!(wheel_units(-1,&mut carry),0);
        assert_eq!(carry,0.0,"a reversal must not leave a phantom scroll behind");
    }

    fn injected(state:&HookState, mem:&mut HookMemory, tx:&Sender<Captured>, x:f64)->bool{
        handle(Raw::Move{x,y:540.0,dx:0.0,dy:0.0,relative:false},state,mem,tx)
    }

    #[test]
    fn the_receiver_hands_back_at_the_border_facing_the_driver() {
        // The peer sits to the left, so control returns at x=0 - not at the far right border, which
        // the injected cursor reaches whenever the user simply keeps moving right.
        let (state,mut mem,tx,rx)=harness(Edge::Left,SCREEN);
        state.receiving.store(true,Ordering::SeqCst);
        state.injection.store(true,Ordering::SeqCst);
        assert!(injected(&state,&mut mem,&tx,960.0));
        assert!(injected(&state,&mut mem,&tx,1919.0));
        assert!(rx.try_recv().is_err(),"the far border must not hand control back");
        assert!(state.receiving.load(Ordering::SeqCst));
        assert!(injected(&state,&mut mem,&tx,1.0));
        assert!(matches!(rx.try_recv(),Ok(Captured::ReturnLocal)));
        assert!(!state.receiving.load(Ordering::SeqCst));
    }

    #[test]
    fn a_cursor_resting_on_the_handback_border_does_not_bounce_control() {
        let (state,mut mem,tx,rx)=harness(Edge::Left,SCREEN);
        state.receiving.store(true,Ordering::SeqCst);
        state.injection.store(true,Ordering::SeqCst);
        assert!(injected(&state,&mut mem,&tx,0.0));
        assert!(rx.try_recv().is_err(),"the first move after a hand-back must not hand straight back");
        assert!(injected(&state,&mut mem,&tx,400.0));
        assert!(injected(&state,&mut mem,&tx,1.0));
        assert!(matches!(rx.try_recv(),Ok(Captured::ReturnLocal)));
    }
}
