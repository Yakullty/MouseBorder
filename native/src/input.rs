use crate::protocol::{InputEvent, MouseButton};
use rdev::{simulate, Button, EventType, Key};
#[cfg(not(target_os="macos"))] use rdev::{grab, Event};
#[cfg(not(target_os="macos"))] use std::sync::Mutex;
use std::{sync::{atomic::{AtomicBool, AtomicU32, Ordering}, mpsc::Sender, Arc}, time::Duration};

#[derive(Debug, Clone)]
pub enum Captured { EnterRemote, ReturnLocal, Event(InputEvent) }

/// rdev hands over absolute pointer positions, so while this machine drives the peer the hidden
/// local cursor is only a motion accumulator: a border that clamps it makes the delta zero and
/// freezes the peer cursor. Within this many points of a non-trigger border the ghost cursor is
/// pulled back inside so physical travel keeps producing deltas.
const REANCHOR_MARGIN: f64 = 40.0;
/// Travel room restored by one re-anchor, in points.
const REANCHOR_PULL: f64 = 320.0;

/// Which border hands control over: the peer screen sits on that side of this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Edge { #[default] Right, Left, Top, Bottom }
impl Edge {
    pub fn parse(s:&str)->Option<Self>{Some(match s{"right"=>Self::Right,"left"=>Self::Left,"top"=>Self::Top,"bottom"=>Self::Bottom,_=>return None})}
    pub fn as_str(self)->&'static str{match self{Self::Right=>"right",Self::Left=>"left",Self::Top=>"top",Self::Bottom=>"bottom"}}
    pub fn code(self)->u32{match self{Self::Right=>0,Self::Left=>1,Self::Top=>2,Self::Bottom=>3}}
    pub fn from_code(c:u32)->Self{match c{1=>Self::Left,2=>Self::Top,3=>Self::Bottom,_=>Self::Right}}
    /// True when the handoff band sits at coordinate zero, so the hand-back band is the far border.
    pub fn low(self)->bool{matches!(self,Self::Left|Self::Top)}
    fn vertical(self)->bool{matches!(self,Self::Top|Self::Bottom)}
    /// The pointer coordinate this edge watches.
    pub fn coord(self,x:f64,y:f64)->f64{if self.vertical(){y}else{x}}
    /// Coordinate of the handoff band along the watched axis.
    pub fn band(self,w:f64,h:f64)->f64{(if self.vertical(){h}else{w})-2.0}
}

#[derive(Clone)]
pub struct HookState {
    pub enabled: Arc<AtomicBool>,
    pub remote: Arc<AtomicBool>,
    pub receiving: Arc<AtomicBool>,
    pub injection: Arc<AtomicBool>,
    pub cursor_hidden: Arc<AtomicBool>,
    pub edge_code: Arc<AtomicU32>,
    pub screen_w: Arc<AtomicU32>,
    pub screen_h: Arc<AtomicU32>,
}
impl HookState {
    pub fn new() -> Self { Self { enabled:Arc::new(AtomicBool::new(false)), remote:Arc::new(AtomicBool::new(false)), receiving:Arc::new(AtomicBool::new(false)), injection:Arc::new(AtomicBool::new(false)), cursor_hidden:Arc::new(AtomicBool::new(false)), edge_code:Arc::new(AtomicU32::new(Edge::Right.code())), screen_w:Arc::new(AtomicU32::new(0)), screen_h:Arc::new(AtomicU32::new(0)) } }
    pub fn set_screen(&self,w:u32,h:u32){ self.screen_w.store(w,Ordering::Relaxed); self.screen_h.store(h,Ordering::Relaxed); }
    pub fn edge(&self)->Edge{ Edge::from_code(self.edge_code.load(Ordering::Relaxed)) }
    pub fn set_edge(&self,e:Edge){ self.edge_code.store(e.code(),Ordering::Relaxed); }
    /// Where the hidden ghost cursor parks while this machine drives the peer.
    pub fn anchor(&self)->(f64,f64){ anchor_point(self.screen_w.load(Ordering::Relaxed) as f64, self.screen_h.load(Ordering::Relaxed) as f64, self.edge()) }
}

#[derive(Default)]
pub(crate) struct HookMemory { last:(f64,f64), ctrl:bool, alt:bool }

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

/// Pointer left the screen through the handoff edge, so control should move to the peer.
pub fn entry_crossed(coord:f64, band:f64, low:bool)->bool { band>0.0 && if low { coord<=2.0 } else { coord>=band } }
/// Pointer reached the hand-back edge, the side opposite the handoff edge.
pub fn return_reached(coord:f64, band:f64, low:bool)->bool { if low { coord>=band-1.0 } else { coord<=2.0 } }

/// Ghost cursor resting point: clear of both trigger bands, with travel room towards the handoff
/// edge and on both borders of the other axis.
pub fn anchor_point(w:f64, h:f64, edge:Edge)->(f64,f64){
    if w<2.0 || h<2.0 { return (0.0,0.0); }
    let room=REANCHOR_MARGIN+REANCHOR_PULL;
    match edge {
        Edge::Left => (room.min(w*0.5), h*0.5),
        Edge::Right => ((w-1.0-room).max(w*0.5), h*0.5),
        Edge::Top => (w*0.5, room.min(h*0.5)),
        Edge::Bottom => (w*0.5, (h-1.0-room).max(h*0.5)),
    }
}

/// The obstacle border the ghost cursor is pressed against, if any. The handoff and hand-back bands
/// are deliberately left alone: they are triggers, not obstacles.
pub fn reanchor(x:f64, y:f64, w:f64, h:f64, edge:Edge)->Option<(f64,f64)>{
    if w<2.0 || h<2.0 { return None; }
    let (ax,ay)=anchor_point(w,h,edge);
    let m=REANCHOR_MARGIN;
    // On the watched axis only the border the pointer came through blocks it; the opposite border is
    // the hand-back trigger and has to stay reachable. Both borders of the other axis block.
    let x_stuck=match edge{Edge::Left=>x<=m,Edge::Right=>x>=w-1.0-m,Edge::Top|Edge::Bottom=>x<=m||x>=w-1.0-m};
    let y_stuck=match edge{Edge::Top=>y<=m,Edge::Bottom=>y>=h-1.0-m,Edge::Left|Edge::Right=>y<=m||y>=h-1.0-m};
    if !x_stuck && !y_stuck { return None; }
    Some((if x_stuck {ax} else {x}, if y_stuck {ay} else {y}))
}

/// Peer cursor position after one forwarded move, clamped to the peer screen so it can come to rest
/// on the hand-back edge and release control from there.
pub fn clamp_move(current:f64, delta:f64, extent:f64)->f64{ if extent<=0.0 {0.0} else {(current+delta).clamp(0.0,extent-1.0)} }

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
            let band=edge.band(state.screen_w.load(Ordering::Relaxed) as f64,state.screen_h.load(Ordering::Relaxed) as f64);
            let low=edge.low();
            if state.injection.load(Ordering::Relaxed) {
                if state.receiving.load(Ordering::Relaxed) && return_reached(edge.coord(x,y),band,low) { state.receiving.store(false,Ordering::SeqCst); let _=tx.send(Captured::ReturnLocal); }
                return true;
            }
            if state.receiving.load(Ordering::Relaxed) { return false; }
            let remote=state.remote.load(Ordering::Relaxed);
            // While driving the peer the position is a virtual accumulator fed by device deltas, so
            // a border that clamps the real cursor cannot stall the peer cursor.
            let (x,y)=if remote && relative {(mem.last.0+dx,mem.last.1+dy)} else {(x,y)};
            if !remote && entry_crossed(edge.coord(x,y),band,low) {
                state.remote.store(true,Ordering::SeqCst);
                let (ax,ay)=state.anchor();
                hide_pointer(state);mem.last=(ax,ay);warp_pointer(state,ax,ay);
                let _=tx.send(Captured::EnterRemote);return false;
            }
            if remote {
                if return_reached(edge.coord(x,y),band,low) { state.remote.store(false,Ordering::SeqCst);restore_pointer(state,x,y);let _=tx.send(Captured::ReturnLocal);return false; }
                let fdx=x-mem.last.0;let fdy=y-mem.last.1;
                if fdx!=0.0||fdy!=0.0 {let _=tx.send(Captured::Event(InputEvent::Move{dx:fdx,dy:fdy}));}
                mem.last = match reanchor(x,y,state.screen_w.load(Ordering::Relaxed) as f64,state.screen_h.load(Ordering::Relaxed) as f64,edge) {
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
            EventType::Wheel{delta_x,delta_y} => Raw::Wheel{dx:delta_x,dy:delta_y},
            _ => Raw::Other,
        };
        let Ok(mut mem)=memory.lock() else { return Some(event) };
        if handle(raw,&state,&mut mem,&tx) { Some(event) } else { None }
    }).map_err(|e|format!("global input hook failed: {e:?}"))
}

fn to_wire_button(b:Button)->Option<MouseButton>{match b{Button::Left=>Some(MouseButton::Left),Button::Right=>Some(MouseButton::Right),Button::Middle=>Some(MouseButton::Middle),_=>None}}

pub fn inject(event:&InputEvent, cursor:&mut (f64,f64), screen:(f64,f64), state:&HookState)->Result<(),String>{
    let native=match event {
        InputEvent::Move{dx,dy}=>{cursor.0=clamp_move(cursor.0,*dx,screen.0);cursor.1=clamp_move(cursor.1,*dy,screen.1);EventType::MouseMove{x:cursor.0,y:cursor.1}},
        InputEvent::Button{button,down}=>{let b=match button{MouseButton::Left=>Button::Left,MouseButton::Right=>Button::Right,MouseButton::Middle=>Button::Middle};if *down{EventType::ButtonPress(b)}else{EventType::ButtonRelease(b)}},
        InputEvent::Wheel{dx,dy}=>EventType::Wheel{delta_x:*dx,delta_y:*dy},
        InputEvent::Key{code,down}=>{let Some(k)=parse_key(code) else{return Ok(())};if *down{EventType::KeyPress(k)}else{EventType::KeyRelease(k)}},
    };
    state.injection.store(true,Ordering::SeqCst);
    let result=simulate(&native).map_err(|e|format!("input injection failed: {e:?}"));
    std::thread::sleep(Duration::from_millis(3));
    state.injection.store(false,Ordering::SeqCst);
    result
}

/// Key names on the wire are rdev's own variant spellings; `parse_key` is the accepting half.
fn key_name(k:Key)->String{format!("{k:?}")}

fn parse_key(s:&str)->Option<Key>{Some(match s{
"Escape"=>Key::Escape,"Return"=>Key::Return,"Space"=>Key::Space,"Tab"=>Key::Tab,"Backspace"=>Key::Backspace,"Delete"=>Key::Delete,"ShiftLeft"=>Key::ShiftLeft,"ShiftRight"=>Key::ShiftRight,"ControlLeft"=>Key::ControlLeft,"ControlRight"=>Key::ControlRight,"Alt"=>Key::Alt,"AltGr"=>Key::AltGr,"MetaLeft"=>Key::MetaLeft,"MetaRight"=>Key::MetaRight,"LeftArrow"=>Key::LeftArrow,"RightArrow"=>Key::RightArrow,"UpArrow"=>Key::UpArrow,"DownArrow"=>Key::DownArrow,
"KeyA"=>Key::KeyA,"KeyB"=>Key::KeyB,"KeyC"=>Key::KeyC,"KeyD"=>Key::KeyD,"KeyE"=>Key::KeyE,"KeyF"=>Key::KeyF,"KeyG"=>Key::KeyG,"KeyH"=>Key::KeyH,"KeyI"=>Key::KeyI,"KeyJ"=>Key::KeyJ,"KeyK"=>Key::KeyK,"KeyL"=>Key::KeyL,"KeyM"=>Key::KeyM,"KeyN"=>Key::KeyN,"KeyO"=>Key::KeyO,"KeyP"=>Key::KeyP,"KeyQ"=>Key::KeyQ,"KeyR"=>Key::KeyR,"KeyS"=>Key::KeyS,"KeyT"=>Key::KeyT,"KeyU"=>Key::KeyU,"KeyV"=>Key::KeyV,"KeyW"=>Key::KeyW,"KeyX"=>Key::KeyX,"KeyY"=>Key::KeyY,"KeyZ"=>Key::KeyZ,"Num0"=>Key::Num0,"Num1"=>Key::Num1,"Num2"=>Key::Num2,"Num3"=>Key::Num3,"Num4"=>Key::Num4,"Num5"=>Key::Num5,"Num6"=>Key::Num6,"Num7"=>Key::Num7,"Num8"=>Key::Num8,"Num9"=>Key::Num9,"Minus"=>Key::Minus,"Equal"=>Key::Equal,"Comma"=>Key::Comma,"Dot"=>Key::Dot,"Slash"=>Key::Slash,"SemiColon"=>Key::SemiColon,"Quote"=>Key::Quote,"BackQuote"=>Key::BackQuote,"LeftBracket"=>Key::LeftBracket,"RightBracket"=>Key::RightBracket,"BackSlash"=>Key::BackSlash,"Home"=>Key::Home,"End"=>Key::End,"PageUp"=>Key::PageUp,"PageDown"=>Key::PageDown,"F1"=>Key::F1,"F2"=>Key::F2,"F3"=>Key::F3,"F4"=>Key::F4,"F5"=>Key::F5,"F6"=>Key::F6,"F7"=>Key::F7,"F8"=>Key::F8,"F9"=>Key::F9,"F10"=>Key::F10,"F11"=>Key::F11,"F12"=>Key::F12,_=>return None})}

pub fn release_key(code:&str)->Result<(),String>{if let Some(k)=parse_key(code){simulate(&EventType::KeyRelease(k)).map_err(|e|format!("release key failed: {e:?}"))?;}Ok(())}
pub fn release_button(button:MouseButton)->Result<(),String>{let b=match button{MouseButton::Left=>Button::Left,MouseButton::Right=>Button::Right,MouseButton::Middle=>Button::Middle};simulate(&EventType::ButtonRelease(b)).map_err(|e|format!("release mouse button failed: {e:?}"))}

fn hide_pointer(state:&HookState){if !state.cursor_hidden.swap(true,Ordering::SeqCst){platform_hide();}}
pub fn restore_pointer(state:&HookState,x:f64,y:f64){if state.cursor_hidden.swap(false,Ordering::SeqCst){platform_show();let edge=state.edge();let band=edge.band(state.screen_w.load(Ordering::Relaxed) as f64,state.screen_h.load(Ordering::Relaxed) as f64);let inside=if edge.low(){3.0}else{band-1.0};let (rx,ry)=if edge.vertical(){(x,inside)}else{(inside,y)};warp_pointer(state,rx,ry);}}
fn warp_pointer(state:&HookState,x:f64,y:f64){state.injection.store(true,Ordering::SeqCst);platform_warp(x,y);std::thread::sleep(Duration::from_millis(3));state.injection.store(false,Ordering::SeqCst);}
#[cfg(target_os="macos")]
fn platform_warp(x:f64,y:f64){#[repr(C)]struct Point{x:f64,y:f64}#[link(name="CoreGraphics",kind="framework")]unsafe extern "C"{fn CGWarpMouseCursorPosition(p:Point)->i32;}unsafe{let _=CGWarpMouseCursorPosition(Point{x,y});}}
#[cfg(target_os="macos")]
fn platform_hide(){#[link(name="CoreGraphics",kind="framework")]unsafe extern "C"{fn CGMainDisplayID()->u32;fn CGDisplayHideCursor(id:u32)->i32;}unsafe{let _=CGDisplayHideCursor(CGMainDisplayID());}}
#[cfg(target_os="macos")]
fn platform_show(){#[link(name="CoreGraphics",kind="framework")]unsafe extern "C"{fn CGMainDisplayID()->u32;fn CGDisplayShowCursor(id:u32)->i32;}unsafe{let _=CGDisplayShowCursor(CGMainDisplayID());}}
#[cfg(target_os="windows")]
fn platform_warp(x:f64,y:f64){#[link(name="user32")]unsafe extern "system"{fn SetCursorPos(x:i32,y:i32)->i32;}unsafe{let _=SetCursorPos(x as i32,y as i32);}}
#[cfg(target_os="windows")]
fn platform_hide(){#[link(name="user32")]unsafe extern "system"{fn ShowCursor(show:i32)->i32;}unsafe{let _=ShowCursor(0);}}
#[cfg(target_os="windows")]
fn platform_show(){#[link(name="user32")]unsafe extern "system"{fn ShowCursor(show:i32)->i32;}unsafe{let _=ShowCursor(1);}}
#[cfg(not(any(target_os="macos",target_os="windows")))]
fn platform_warp(_:f64,_:f64){}
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

    #[test]
    fn handoff_and_handback_are_opposite_borders() {
        for w in [1920.0,800.0] {
            let band=w-2.0;
            for low in [false,true] {
                assert!(!entry_crossed(w*0.5,band,low));
                let (handoff,other)=if low {(0.0,band)} else {(band,0.0)};
                assert!(entry_crossed(handoff,band,low),"low={low}");
                assert!(return_reached(other,band,low),"low={low}");
                assert!(!return_reached(handoff,band,low),"the handoff border must not also hand back");
            }
        }
    }

    #[test]
    fn zero_edge_disables_handoff() { assert!(!entry_crossed(1919.0,0.0,false)); assert!(!entry_crossed(0.0,0.0,true)); }

    /// A point is on the hand-back border when reaching it should release control, not re-anchor.
    fn hand_back(edge:Edge,x:f64,y:f64,w:f64,h:f64)->bool{ return_reached(edge.coord(x,y),edge.band(w,h),edge.low()) }

    /// Neither trigger band may catch the pointer on its own, or the ghost cursor would hand control
    /// over or back the moment it parks.
    fn assert_out_of_both_bands(edge:Edge,x:f64,y:f64,w:f64,h:f64,what:&str){
        let band=edge.band(w,h);
        assert!(!entry_crossed(edge.coord(x,y),band,edge.low()),"{what} would enter the peer");
        assert!(!hand_back(edge,x,y,w,h),"{what} would hand back");
    }

    #[test]
    fn trigger_bands_are_not_re_anchored() {
        let (w,h)=(1920.0,1080.0);
        assert_eq!(reanchor(6.0,540.0,w,h,Edge::Right),None,"a flick left has to stay reachable");
        assert_eq!(reanchor(1913.0,540.0,w,h,Edge::Left),None,"a flick right has to stay reachable");
        assert_eq!(reanchor(960.0,1073.0,w,h,Edge::Top),None,"a flick down has to stay reachable");
        assert_eq!(reanchor(960.0,7.0,w,h,Edge::Bottom),None,"a flick up has to stay reachable");
        assert_eq!(reanchor(960.0,540.0,w,h,Edge::Right),None);
    }

    #[test]
    fn obstacle_borders_are_re_anchored_with_room() {
        let (w,h)=(1920.0,1080.0);
        assert_eq!(reanchor(w-1.0,540.0,w,h,Edge::Right),Some((1559.0,540.0)));
        assert_eq!(reanchor(3.0,540.0,w,h,Edge::Left),Some((360.0,540.0)));
        assert_eq!(reanchor(960.0,3.0,w,h,Edge::Right),Some((960.0,540.0)));
        assert_eq!(reanchor(960.0,h-1.0,w,h,Edge::Right),Some((960.0,540.0)));
        assert_eq!(reanchor(w-1.0,3.0,w,h,Edge::Right),Some((1559.0,540.0)));
        assert_eq!(reanchor(960.0,3.0,w,h,Edge::Top),Some((960.0,360.0)),"the border it came through pulls along the watched axis");
        assert_eq!(reanchor(960.0,h-1.0,w,h,Edge::Top),None,"the far border hands back instead of re-anchoring");
        assert_eq!(reanchor(3.0,540.0,w,h,Edge::Top),Some((960.0,540.0)),"the other axis pulls to its middle");
    }

    #[test]
    fn anchors_and_targets_stay_out_of_every_trigger_band() {
        for (w,h) in [(1920.0,1080.0),(1024.0,768.0),(500.0,400.0)] {
            for edge in [Edge::Right,Edge::Left,Edge::Top,Edge::Bottom] {
                let (ax,ay)=anchor_point(w,h,edge);
                assert!(ax>2.0 && ax<w-2.0 && ay>2.0 && ay<h-2.0,"{w}x{h} {edge:?} parked at ({ax},{ay})");
                assert_out_of_both_bands(edge,ax,ay,w,h,"the anchor");
                for (x,y) in [(1.0,h*0.5),(w-1.0,h*0.5),(w*0.5,1.0),(w*0.5,h-1.0)] {
                    if hand_back(edge,x,y,w,h) {
                        assert_eq!(reanchor(x,y,w,h,edge),None,"{edge:?} must keep its hand-back border a pure trigger");
                    } else {
                        let t=reanchor(x,y,w,h,edge).expect("obstacle border must re-anchor");
                        assert_out_of_both_bands(edge,t.0,t.1,w,h,"the re-anchor target");
                    }
                }
            }
        }
    }

    #[test]
    fn degenerate_screens_are_ignored() {
        assert_eq!(reanchor(0.0,0.0,0.0,0.0,Edge::Right),None);
        assert_eq!(anchor_point(0.0,0.0,Edge::Top),(0.0,0.0));
    }

    #[test]
    fn peer_cursor_clamps_at_the_screen_border() {
        assert_eq!(clamp_move(1919.0,5000.0,1920.0),1919.0);
        assert_eq!(clamp_move(1919.0,-3000.0,1920.0),0.0);
        assert_eq!(clamp_move(500.0,120.0,1920.0),620.0);
        assert_eq!(clamp_move(500.0,10.0,0.0),0.0);
    }
}
