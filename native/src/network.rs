use crate::{clipboard, input::{self, Captured, HookState}, protocol::{InputEvent, Message}};
use display_info::DisplayInfo;
use futures_util::{stream::SplitStream, FutureExt, SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use rand::{thread_rng, Rng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{collections::{HashSet, VecDeque}, path::{Path, PathBuf}, sync::{atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering}, mpsc, Arc, Mutex, RwLock}, thread, time::Duration};
use tauri::{AppHandle, Emitter, Manager};
use tokio::{net::TcpListener, sync::mpsc as tmpsc};
use tokio_tungstenite::{accept_async, connect_async_with_config, tungstenite::Message as WsMessage, WebSocketStream};
type HmacSha256 = Hmac<Sha256>;

const DEFAULT_PORT: u16 = 39876;
/// Both ends dial this fixed port. The overrides exist so a single machine can host a second
/// instance for local end-to-end testing.
const PORT_ENV: &str = "MOUSEBORDER_PORT";
const PEER_PORT_ENV: &str = "MOUSEBORDER_PEER_PORT";
const PIN_ENV: &str = "MOUSEBORDER_PIN";

#[derive(Clone, Serialize)]
pub struct UiState { pub hostname:String, pub pin:String, pub status:String, pub peer:String, pub edge:String, pub error:String, pub notice:String, pub edge_enabled:bool, pub clipboard_enabled:bool, pub hook:String, pub control:String, pub devices:Vec<Device> }
#[derive(Serialize, Deserialize, Clone)]
struct Packet { mac:String, body:Message }

/// One unit on the wire. Control travels as signed JSON text; bulk clipboard bytes travel as
/// signed binary frames (`[32-byte MAC][kind][transfer LE][payload]`) so a file copy is not a
/// base64 text blob, which is both cheaper and a less malware-like static profile.
#[derive(Debug, Clone, PartialEq)]
enum Frame { Body(Message), Chunk{kind:u8, transfer:u64, data:Vec<u8>} }
const CHUNK_FILE: u8 = 1;
const CHUNK_IMAGE: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings { pub edge:String, pub edge_enabled:bool, pub clipboard_enabled:bool, #[serde(default)] pub devices:Vec<Device> }
impl Default for Settings { fn default()->Self{ Self{ edge:"right".into(), edge_enabled:true, clipboard_enabled:true, devices:Vec::new() } } }

/// A peer we have paired with. `token` is the shared secret issued at pairing time; presenting it
/// lets either side dial again without the current six-digit code. Forgetting the device on this
/// machine also revokes the token, because incoming token auth is checked against this list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device { pub host:String, pub label:String, pub token:String }

#[derive(Clone, Debug, PartialEq, Eq)]
struct Target { host:String, pin:String, token:Option<String> }

pub struct Core {
    pub ui:Mutex<UiState>, pin:RwLock<String>, out:Mutex<Option<tmpsc::UnboundedSender<Frame>>>,
    pub hook:HookState, app:Mutex<Option<AppHandle>>, clipboard_echo:Mutex<String>, pub edge_enabled:AtomicBool, pub clipboard_enabled:AtomicBool,
    target:Mutex<Option<Target>>, link:AtomicUsize, devices:Mutex<Vec<Device>>, transfer:AtomicU64,
    capture:Mutex<Option<Arc<mpsc::Sender<Captured>>>>, hook_state:AtomicU8,
}
/// Hook thread outcomes, mirrored into `UiState.hook` for the settings UI.
const HOOK_STOPPED: u8 = 0; const HOOK_RUNNING: u8 = 1; const HOOK_FAILED: u8 = 2;
impl Core {
    pub fn new()->Arc<Self>{
        let hostname=std::env::var("COMPUTERNAME").or_else(|_|std::env::var("HOSTNAME")).unwrap_or_else(|_|"MouseBorder".into());
        let pin=initial_pin(); let (sx,sy,sw,sh)=screen_rect(); let hook=HookState::new();
        hook.set_screen(sx,sy,sw,sh);
        let defaults=Settings::default();
        Arc::new(Self{ui:Mutex::new(UiState{hostname,pin:pin.clone(),status:"waiting".into(),peer:String::new(),edge:defaults.edge.clone(),error:String::new(),notice:String::new(),edge_enabled:defaults.edge_enabled,clipboard_enabled:defaults.clipboard_enabled,hook:"stopped".into(),control:"local".into(),devices:Vec::new()}),pin:RwLock::new(pin),out:Mutex::new(None),hook,app:Mutex::new(None),clipboard_echo:Mutex::new(String::new()),edge_enabled:AtomicBool::new(defaults.edge_enabled),clipboard_enabled:AtomicBool::new(defaults.clipboard_enabled),target:Mutex::new(None),link:AtomicUsize::new(0),devices:Mutex::new(Vec::new()),transfer:AtomicU64::new(1),capture:Mutex::new(None),hook_state:AtomicU8::new(HOOK_STOPPED)})
    }
    pub fn set_app(&self,a:AppHandle){*self.app.lock().unwrap()=Some(a.clone());if let Some(s)=read_settings(&settings_path(&a)){self.apply_settings(&s);}}
    fn apply_settings(&self,s:&Settings){
        self.edge_enabled.store(s.edge_enabled,Ordering::SeqCst);
        self.clipboard_enabled.store(s.clipboard_enabled,Ordering::SeqCst);
        self.hook.set_edge(input::Edge::parse(&s.edge).unwrap_or_default());
        let connected=self.ui.lock().unwrap().status=="connected";
        self.hook.enabled.store(s.edge_enabled&&connected,Ordering::SeqCst);
        self.update(|u|{u.edge=s.edge.clone();u.edge_enabled=s.edge_enabled;u.clipboard_enabled=s.clipboard_enabled;u.devices=s.devices.clone();});
        *self.devices.lock().unwrap()=s.devices.clone();
    }
    fn persist(&self){
        let s=Settings{edge:self.ui.lock().unwrap().edge.clone(),edge_enabled:self.edge_enabled.load(Ordering::SeqCst),clipboard_enabled:self.clipboard_enabled.load(Ordering::SeqCst),devices:self.devices.lock().unwrap().clone()};
        let Some(app)=self.app.lock().unwrap().clone() else {return};
        write_settings(&settings_path(&app),&s);
    }
    fn update(&self,f:impl FnOnce(&mut UiState)){let mut s=self.ui.lock().unwrap();f(&mut s);if let Some(a)=self.app.lock().unwrap().as_ref(){let _=a.emit("state",s.clone());}}
    pub fn refresh_pin(&self)->String{let p=make_pin();*self.pin.write().unwrap()=p.clone();self.update(|s|s.pin=p.clone());p}
    pub fn set_edge(&self,side:String){if let Some(edge)=input::Edge::parse(&side){self.hook.set_edge(edge);self.update(|s|s.edge=edge.as_str().to_string());self.persist();}}
    pub fn set_edge_enabled(&self,on:bool){self.edge_enabled.store(on,Ordering::SeqCst);self.hook.enabled.store(on&&self.ui.lock().unwrap().status=="connected",Ordering::SeqCst);if !on{input::force_return(&self.hook);self.send(Message::ReturnControl);}self.update(|s|{s.edge_enabled=on;if !on{s.control="local".into();}});self.persist();}
    pub fn set_clipboard_enabled(&self,on:bool){self.clipboard_enabled.store(on,Ordering::SeqCst);self.update(|s|s.clipboard_enabled=on);self.persist();}
    pub fn disconnect(&self){self.link.fetch_add(1,Ordering::SeqCst);*self.target.lock().unwrap()=None;self.send(Message::Disconnect);input::force_return(&self.hook);self.update(|s|{s.status="waiting".into();s.peer.clear();s.control="local".into();s.notice.clear();});}
    /// Ends the retry loop belonging to `gen` after the *peer* asked to disconnect. The CAS is what
    /// makes this safe: a connect or disconnect the user issued in the meantime owns a newer
    /// generation and must not be clobbered by this late cleanup.
    fn stop_retrying(&self,gen:usize){
        if self.link.compare_exchange(gen,gen+1,Ordering::SeqCst,Ordering::SeqCst).is_ok(){
            self.target.lock().unwrap().take();
            self.update(|s|s.notice="对端已断开连接".into());
        }
    }
    fn remember(&self,host:String,label:String,token:String){
        let label=if label.trim().is_empty(){host.clone()}else{label};
        let mut list=self.devices.lock().unwrap();
        match list.iter_mut().find(|d|d.host==host){Some(d)=>{d.label=label;d.token=token;},None=>list.push(Device{host,label,token})}
        let snapshot=list.clone();drop(list);
        self.update(|s|s.devices=snapshot);
        self.persist();
    }
    pub fn forget_device(&self,host:String){
        self.devices.lock().unwrap().retain(|d|d.host!=host);
        let snapshot=self.devices.lock().unwrap().clone();
        self.update(|s|s.devices=snapshot);
        self.persist();
    }
    fn token_for(&self,host:&str)->Option<String>{self.devices.lock().unwrap().iter().find(|d|d.host==host).map(|d|d.token.clone())}
    /// Dials a remembered device using its stored token, so no code has to be typed again.
    pub fn connect_device(self:&Arc<Self>,host:String){
        let Some(token)=self.token_for(&host) else {self.update(|s|s.error="没有这台设备的授权记录，请用配对码连接".into());return};
        self.dial(Target{host,pin:String::new(),token:Some(token)});
    }
    pub fn send(&self,m:Message){self.send_frame(Frame::Body(m));}
    fn send_frame(&self,f:Frame){if let Some(tx)=self.out.lock().unwrap().as_ref(){let _=tx.send(f);}}
    fn send_chunk(&self,kind:u8,transfer:u64,data:Vec<u8>){self.send_frame(Frame::Chunk{kind,transfer,data});}
    pub fn start(self:&Arc<Self>){
        let c=self.clone();tauri::async_runtime::spawn(async move{if let Err(e)=server(c.clone()).await{c.update(|s|s.error=format!("本机监听启动失败：{e}"));}});
        let (tx,rx)=mpsc::channel();*self.capture.lock().unwrap()=Some(Arc::new(tx));
        let c=self.clone();thread::spawn(move||while let Ok(ev)=rx.recv(){match ev{Captured::EnterRemote=>{c.update(|s|s.control="remote".into());c.send(Message::Control{enabled:true})},Captured::ReturnLocal=>{c.update(|s|s.control="local".into());c.send(Message::ReturnControl)},Captured::Event(event)=>c.send(Message::Input{event})}});
        // Displays can be plugged in or rearranged while the app runs; the handoff edges follow.
        let c=self.clone();thread::spawn(move||loop{let(x,y,w,h)=screen_rect();c.hook.set_screen(x,y,w,h);thread::sleep(Duration::from_secs(2));});
        self.spawn_hook();
        let c=self.clone();thread::spawn(move||{let mut previous=String::new();loop{
            if c.clipboard_enabled.load(Ordering::Relaxed){
                if let Ok(mut clip)=arboard::Clipboard::new(){
                    if let Some((sig,payload))=clipboard::read(&mut clip){
                        if sig!=previous{
                            previous=sig.clone();
                            if sig!=*c.clipboard_echo.lock().unwrap(){c.send_payload(&payload);}
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(500));
        }});
    }
    fn send_payload(&self,payload:&clipboard::Payload){
        match payload{
            clipboard::Payload::Text(text)=>self.send(Message::Clipboard{text:text.clone()}),
            clipboard::Payload::Image(png)=>{
                let transfer=self.transfer.fetch_add(1,Ordering::SeqCst);
                self.send(Message::ClipboardImage{transfer,size:png.len() as u64});
                clipboard::split_chunks(png,|data|self.send_chunk(CHUNK_IMAGE,transfer,data));
            },
            clipboard::Payload::Files(paths)=>match clipboard::plan(paths){
                Err(e)=>self.update(|s|s.error=e),
                Ok((files,paths))=>{
                    let transfer=self.transfer.fetch_add(1,Ordering::SeqCst);
                    self.send(Message::ClipboardFiles{transfer,files});
                    for path in paths{
                        if let Err(e)=clipboard::read_chunks(&path,|data|self.send_chunk(CHUNK_FILE,transfer,data)){
                            self.update(|s|s.error=format!("读取文件失败：{e}"));break;
                        }
                    }
                }
            },
        }
    }
    /// Writes a received payload and records what the clipboard actually holds afterwards, so the
    /// poller sees our own write as an echo instead of a fresh copy to send back.
    fn receive_payload(self:&Arc<Self>,payload:clipboard::Payload){
        let c=self.clone();
        let _=tauri::async_runtime::spawn_blocking(move||{
            let Ok(mut clip)=arboard::Clipboard::new() else {return};
            *c.clipboard_echo.lock().unwrap()=clipboard::signature(&payload);
            if clipboard::write(&mut clip,&payload).is_ok(){
                if let Some((sig,_))=clipboard::read(&mut clip){*c.clipboard_echo.lock().unwrap()=sig;}
            }
        });
    }
    /// (Re)creates the suppressing system hook. macOS consults the permission when the tap is
    /// created, so a grant made while this process was already running only takes effect here.
    fn spawn_hook(self:&Arc<Self>){
        let Some(tx)=self.capture.lock().unwrap().clone() else {return};
        self.hook_state.store(HOOK_RUNNING,Ordering::SeqCst);
        self.update(|s|{s.hook="running".into();if s.error.starts_with(HOOK_ERROR_PREFIX){s.error.clear();}});
        let c=self.clone();
        thread::spawn(move||match input::start_hook((*tx).clone(),c.hook.clone()){
            Ok(())=>{c.hook_state.store(HOOK_STOPPED,Ordering::SeqCst);c.update(|s|s.hook="stopped".into());},
            Err(e)=>{c.hook_state.store(HOOK_FAILED,Ordering::SeqCst);c.update(|s|{s.hook="failed".into();s.error=hook_hint(&e);});},
        });
    }
    pub fn retry_hook(self:&Arc<Self>){if self.hook_state.load(Ordering::SeqCst)!=HOOK_RUNNING{self.spawn_hook();}}
    pub fn connect(self:&Arc<Self>,host:String,pin:String){
        let (host,pin)=(host.trim().to_string(),pin.trim().to_string());
        let Ok(ip)=host.parse::<std::net::IpAddr>() else {self.update(|s|s.error="请输入有效的 IPv4/IPv6 地址".into());return};
        if !valid_pin(&pin) {self.update(|s|s.error="配对码必须是六位数字".into());return}
        let host=ip.to_string();
        let token=self.token_for(&host);
        self.dial(Target{host,pin,token});
    }
    fn dial(self:&Arc<Self>,target:Target){
        *self.target.lock().unwrap()=Some(target.clone());
        let gen=self.link.fetch_add(1,Ordering::SeqCst)+1;
        self.update(|s|{s.error.clear();s.notice.clear();s.status="connecting".into();s.peer=target.host.clone();});
        let c=self.clone();tauri::async_runtime::spawn(async move{attempt(c,gen,target).await;});
    }
}

const HOOK_ERROR_PREFIX: &str = "输入捕获未启动";
/// rdev only surfaces the library error; what the user needs is which pane to open and the fact
/// that a running process has to retry the tap after granting.
fn hook_hint(e:&str)->String{
    #[cfg(target_os="macos")]
    let detail="macOS 需要在「系统设置 → 隐私与安全性 → 辅助功能」和「输入监控」两处同时放行 MouseBorder，然后点“重试捕获”。未签名的包每次重新构建都会更换签名身份，已授予的权限会随之失效。";
    #[cfg(not(target_os="macos"))]
    let detail="Windows 请以当前用户身份重新运行本程序；仍失败时检查安全软件是否拦截了全局钩子。";
    if e.contains("EventTapError")||e.contains("HookError"){format!("{HOOK_ERROR_PREFIX}：{detail}（原始错误：{e}）")}else{format!("{HOOK_ERROR_PREFIX}：{e}")}
}

/// Dials `target` and keeps redialing with capped backoff until the user disconnects, a newer
/// connect supersedes this attempt, or the pairing code is rejected.
async fn attempt(core:Arc<Core>, gen:usize, target:Target){
    let mut delay=1u64;
    loop{
        if stale(&core,gen,&target){return}
        if let Err(e)=connect_peer(core.clone(),&target).await{
            core.update(|s|s.error=e.clone());
            if fatal_pairing(&e){core.target.lock().unwrap().take();core.update(|s|{s.status="waiting".into();s.peer.clear();});return}
        }
        if stale(&core,gen,&target){return}
        core.update(|s|if s.status!="connected"{s.status="retrying".into()});
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay=(delay*2).min(10);
    }
}
fn stale(core:&Arc<Core>,gen:usize,target:&Target)->bool{core.link.load(Ordering::SeqCst)!=gen||core.target.lock().unwrap().as_ref()!=Some(target)}
/// A rejected code or a mismatched peer will not fix itself, so those end the retry loop and leave
/// the message up for the user to act on.
fn fatal_pairing(e:&str)->bool{e.contains("配对码")||e.contains("响应")||e.contains("会话密钥")}

fn make_pin()->String{format!("{:06}",thread_rng().gen_range(0..1_000_000))}
fn valid_pin(p:&str)->bool{p.len()==6&&p.bytes().all(|b|b.is_ascii_digit())}
fn make_key()->String{let mut b=[0u8;32];thread_rng().fill_bytes(&mut b);hex(&b)}
fn env_port(name:&str)->Option<u16>{std::env::var(name).ok().and_then(|v|v.parse().ok())}
fn listen_port()->u16{env_port(PORT_ENV).unwrap_or(DEFAULT_PORT)}
fn peer_port()->u16{env_port(PEER_PORT_ENV).or_else(||env_port(PORT_ENV)).unwrap_or(DEFAULT_PORT)}
fn initial_pin()->String{std::env::var(PIN_ENV).ok().filter(|v|valid_pin(v)).unwrap_or_else(make_pin)}
/// Union of display bounds `(x, y, right, bottom)`. A monitor left of or above the primary has a
/// negative origin, and the handoff edges belong to the whole desktop rather than to one screen.
fn union_rect(bounds:impl Iterator<Item=(i32,i32,i32,i32)>)->(i32,i32,u32,u32){
    let (mut x0,mut y0,mut x1,mut y1)=(i32::MAX,i32::MAX,i32::MIN,i32::MIN);
    for (x,y,r,b) in bounds { x0=x0.min(x); y0=y0.min(y); x1=x1.max(r); y1=y1.max(b); }
    if x1<=x0||y1<=y0 { return (0,0,1920,1080); }
    (x0,y0,(x1-x0) as u32,(y1-y0) as u32)
}
fn screen_rect()->(i32,i32,u32,u32){union_rect(DisplayInfo::all().unwrap_or_default().iter().map(|d|(d.x,d.y,d.x+d.width as i32,d.y+d.height as i32)))}

pub fn settings_path(app:&AppHandle)->PathBuf{app.path().app_data_dir().unwrap_or_else(|_|std::env::temp_dir()).join("settings.json")}
pub fn read_settings(path:&Path)->Option<Settings>{
    let mut s=serde_json::from_str::<Settings>(&std::fs::read_to_string(path).ok()?).ok()?;
    if input::Edge::parse(&s.edge).is_none(){s.edge=Settings::default().edge}
    Some(s)
}
pub fn write_settings(path:&Path,s:&Settings){
    if let Some(dir)=path.parent(){let _=std::fs::create_dir_all(dir);}
    if let Ok(json)=serde_json::to_vec_pretty(s){let _=std::fs::write(path,json);}
}
async fn server(core:Arc<Core>)->Result<(),String>{
    let listener=TcpListener::bind(("0.0.0.0",listen_port())).await.map_err(|e|e.to_string())?;
    loop{let(stream,addr)=listener.accept().await.map_err(|e|e.to_string())?;let _=stream.set_nodelay(true);let c=core.clone();tauri::async_runtime::spawn(async move{
        if c.ui.lock().unwrap().status!="waiting"{return;}
        let host=addr.ip().to_string();
        let Ok(mut ws)=accept_async(stream).await else{return};let Some(Ok(WsMessage::Text(txt)))=ws.next().await else{return};
        let Ok(Message::Pair{pin,label,token})=serde_json::from_str(&txt) else{return};
        // A remembered token stands in for the code, but only one this machine still holds: that is
        // what makes 忘记 on our side revoke the peer's ability to dial back in without a code.
        let known=token.filter(|t|!t.is_empty()&&c.token_for(&host).as_deref()==Some(t.as_str()));
        let trusted=known.is_some();
        if !trusted&&pin!=*c.pin.read().unwrap(){let bad=Message::PairResult{ok:false,key:None,label:None,trust:None};let _=ws.send(WsMessage::Text(serde_json::to_string(&bad).unwrap().into())).await;return;}
        let trust=known.unwrap_or_else(make_key);
        let key=make_key();let reply=Message::PairResult{ok:true,key:Some(key.clone()),label:Some(c.ui.lock().unwrap().hostname.clone()),trust:Some(trust.clone())};
        if ws.send(WsMessage::Text(serde_json::to_string(&reply).unwrap().into())).await.is_ok(){
            c.remember(host,label.clone(),trust);
            install(c,ws,key,label).await;
        }
    });}
}
async fn connect_peer(core:Arc<Core>,target:&Target)->Result<(),String>{
    let addr=std::net::SocketAddr::new(target.host.parse().map_err(|_|"请输入有效的 IPv4/IPv6 地址")?,peer_port());
    // Nagle holds a small mouse frame until the peer acknowledges the previous one, which on a LAN
    // is a 40-200 ms stall per burst and is felt as a stuttering pointer rather than as latency.
    let(mut ws,_)=connect_async_with_config(format!("ws://{addr}"),None,true).await.map_err(|e|e.to_string())?;
    let pair=Message::Pair{pin:target.pin.clone(),label:core.ui.lock().unwrap().hostname.clone(),token:target.token.clone()};
    ws.send(WsMessage::Text(serde_json::to_string(&pair).unwrap().into())).await.map_err(|e|e.to_string())?;
    let Some(Ok(WsMessage::Text(reply)))=ws.next().await else{return Err("对端未返回配对结果".into())};
    let Message::PairResult{ok,key,label,trust}=serde_json::from_str(&reply).map_err(|_|"对端配对响应格式错误，双方需使用同一版本".to_string())? else{return Err("对端配对响应格式错误，双方需使用同一版本".into())};
    if !ok{return Err(if target.token.is_some(){"配对码错误或授权已失效，请重新用配对码连接".into()}else{"配对码错误或已刷新".into()})}
    let host=target.host.clone();
    if let Some(trust)=trust{core.remember(host,label.clone().unwrap_or_default(),trust);}
    install(core,ws,key.ok_or("对端没有提供会话密钥")?,label.unwrap_or(target.host.clone())).await;
    Ok(())
}
async fn install<S>(core:Arc<Core>,ws:WebSocketStream<S>,key:String,peer:String)
where S:tokio::io::AsyncRead+tokio::io::AsyncWrite+Unpin+Send+'static {
    let(tx,mut rx)=tmpsc::unbounded_channel::<Frame>();
    {let mut out=core.out.lock().unwrap();if out.is_some(){return}*out=Some(tx);}
    // Captured before any await: if the peer hangs up we end exactly this generation's retry loop.
    let gen=core.link.load(Ordering::SeqCst);
    let(mut sink,mut stream)=ws.split();
    core.hook.enabled.store(core.edge_enabled.load(Ordering::SeqCst),Ordering::SeqCst);
    core.update(|s|{s.status="connected".into();s.peer=peer;s.error.clear();s.notice.clear();});
    let k=key.clone();let writer=tauri::async_runtime::spawn(async move{while let Some(frame)=rx.recv().await{
        let sent=match frame{
            Frame::Body(body)=>{let Ok(bytes)=serde_json::to_vec(&body)else{continue};let packet=Packet{mac:hex(&mac_bytes(&k,&bytes)),body};match serde_json::to_string(&packet){Ok(text)=>sink.send(WsMessage::Text(text.into())).await,Err(_)=>continue}}
            Frame::Chunk{kind,transfer,data}=>{let payload=chunk_payload(kind,transfer,&data);let mut bin=mac_bytes(&k,&payload);bin.extend_from_slice(&payload);sink.send(WsMessage::Binary(bin.into())).await}
        };
        if sent.is_err(){break;}
    }});
    let(x,y,w,h)=screen_rect();let mut inj=input::InjectState::new((x as f64+w as f64/2.0,y as f64+h as f64/2.0));let mut held_keys=HashSet::<String>::new();let mut held_buttons=HashSet::new();
    let mut queued:VecDeque<Frame>=VecDeque::new();
    let mut incoming:Option<clipboard::Incoming>=None;
    let mut image:Option<(u64,u64,Vec<u8>)>=None;
    let mut peer_closed=false;
    loop{
        let body=match queued.pop_front(){Some(b)=>b,None=>match next_body(&mut stream,&key).await{Some(b)=>b,None=>break}};
        match body{
        Frame::Body(Message::Input{event})=>{
            let event=match event{
                InputEvent::Move{dx,dy}=>{
                    let mut burst=Vec::new();
                    while let Some(Some(m))=next_body(&mut stream,&key).now_or_never(){burst.push(m);}
                    let(total,left)=coalesce_moves((dx,dy),burst);
                    for m in left.into_iter().rev(){queued.push_front(m);}
                    InputEvent::Move{dx:total.0,dy:total.1}
                }
                other=>other,
            };
            match &event{InputEvent::Key{code,down}=>{if *down{held_keys.insert(code.clone());}else{held_keys.remove(code);}},InputEvent::Button{button,down}=>{if *down{held_buttons.insert(*button);}else{held_buttons.remove(button);}},_=>{}}
            if let Err(e)=input::inject(&event,&mut inj,&core.hook){core.update(|s|s.error=format!("输入注入失败：{e}"));}
        },
        Frame::Body(Message::Control{enabled})=>{let(x,y,w,h)=screen_rect();core.hook.set_screen(x,y,w,h);if enabled{core.hook.receiving_epoch.fetch_add(1,Ordering::SeqCst);}core.hook.receiving.store(enabled,Ordering::SeqCst);core.update(|s|s.control=if enabled{"receiving"}else{"local"}.into());},
        Frame::Body(Message::Disconnect)=>{peer_closed=true;break},
        Frame::Body(Message::ReturnControl)=>{input::force_return(&core.hook);core.update(|s|s.control="local".into());},
        // Awaiting the clipboard thread here would stall every input frame queued behind it.
        Frame::Body(Message::Clipboard{text}) if core.clipboard_enabled.load(Ordering::Relaxed)=>core.receive_payload(clipboard::Payload::Text(text)),
        Frame::Body(Message::ClipboardImage{transfer,size}) if core.clipboard_enabled.load(Ordering::Relaxed)=>{
            if size>clipboard::MAX_TRANSFER{core.update(|s|s.error="图片超过大小上限".into());}
            else{image=Some((transfer,size,Vec::new()));core.update(|s|s.notice="正在接收图片…".into());}
        },
        Frame::Body(Message::ClipboardFiles{transfer,files}) if core.clipboard_enabled.load(Ordering::Relaxed)=>{
            let count=files.len();
            match clipboard::Incoming::new(transfer,files,received_dir(&core,transfer)){
                Ok(inc)=>{incoming=Some(inc);core.update(|s|s.notice=format!("正在接收 {count} 个文件…"));},
                Err(e)=>{incoming=None;core.update(|s|s.error=format!("接收文件失败：{e}"));},
            }
        },
        Frame::Chunk{kind,transfer,data} if core.clipboard_enabled.load(Ordering::Relaxed)=>{
            if kind==CHUNK_FILE{
                let Some(inc)=incoming.as_mut().filter(|i|i.transfer==transfer) else {continue};
                match inc.chunk(&data){
                    Ok(true)=>{let paths=std::mem::take(&mut inc.done);incoming=None;
                        core.update(|s|s.notice=format!("已接收 {} 个文件，可直接粘贴",paths.len()));
                        core.receive_payload(clipboard::Payload::Files(paths));},
                    Ok(false)=>{},
                    Err(e)=>{incoming=None;core.update(|s|s.error=format!("接收文件失败：{e}"));},
                }
            }else if kind==CHUNK_IMAGE{
                let Some((t,size,buf))=image.as_mut().filter(|(t,_,_)|*t==transfer) else {continue};
                if buf.len()+data.len()>*size as usize{image=None;core.update(|s|s.error="图片分块超出声明大小".into());continue;}
                buf.extend_from_slice(&data);
                if buf.len()==*size as usize{let bytes=std::mem::take(buf);image=None;
                    core.update(|s|s.notice="已接收图片，可直接粘贴".into());
                    core.receive_payload(clipboard::Payload::Image(bytes));}
            }
        },_=>{}
    }}
    writer.abort();for key in held_keys{let _=input::release_key(&key);}for button in held_buttons{let _=input::release_button(button);}core.hook.enabled.store(false,Ordering::SeqCst);input::force_return(&core.hook);*core.out.lock().unwrap()=None;core.update(|s|{s.status="waiting".into();s.peer.clear();s.control="local".into();});
    if peer_closed{core.stop_retrying(gen);}
}
/// Received files land under the app's own data directory, one folder per transfer, and the
/// clipboard is then pointed at them so a paste on this machine yields real files.
fn received_dir(core:&Core,transfer:u64)->PathBuf{
    let base=core.app.lock().unwrap().as_ref().and_then(|a|a.path().app_data_dir().ok()).unwrap_or_else(std::env::temp_dir);
    base.join("received").join(transfer.to_string())
}
/// Next verified frame, or `None` once the link ends. Unrecognised frames and failed MACs are skipped.
async fn next_body<S>(stream:&mut SplitStream<WebSocketStream<S>>,key:&str)->Option<Frame>
where S:tokio::io::AsyncRead+tokio::io::AsyncWrite+Unpin {
    loop{
        match stream.next().await?.ok()?{
            WsMessage::Text(text)=>{let Ok(packet)=serde_json::from_str::<Packet>(&text) else{continue};if verify(key,&packet){return Some(Frame::Body(packet.body))}},
            WsMessage::Binary(bin)=>{if let Some(f)=verify_chunk(key,&bin){return Some(f)}},
            _=>continue,
        }
    }
}
fn mac_bytes(key:&str,payload:&[u8])->Vec<u8>{let mut h=HmacSha256::new_from_slice(key.as_bytes()).unwrap();h.update(payload);h.finalize().into_bytes().to_vec()}
fn chunk_payload(kind:u8,transfer:u64,data:&[u8])->Vec<u8>{let mut v=Vec::with_capacity(9+data.len());v.push(kind);v.extend_from_slice(&transfer.to_le_bytes());v.extend_from_slice(data);v}
fn verify_chunk(key:&str,bin:&[u8])->Option<Frame>{
    if bin.len()<32+9{return None}
    let(mac,payload)=bin.split_at(32);
    let mut h=HmacSha256::new_from_slice(key.as_bytes()).ok()?;h.update(payload);h.verify_slice(mac).ok()?;
    let kind=payload[0];let transfer=u64::from_le_bytes(payload[1..9].try_into().ok()?);
    Some(Frame::Chunk{kind,transfer,data:payload[9..].to_vec()})
}
/// Folds the moves that arrived in one burst into a single injection. Every SendInput re-enters
/// this process's own low-level hook, so applying a backlog frame by frame costs a hook round-trip
/// and a visible cursor jump per frame instead of one smooth step. Anything that is not a move is
/// handed back untouched, in order.
fn coalesce_moves(mut total:(f64,f64),burst:Vec<Frame>)->((f64,f64),Vec<Frame>){
    let mut left=Vec::new();
    for m in burst{
        match m{
            Frame::Body(Message::Input{event:InputEvent::Move{dx,dy}}) if left.is_empty()=>{total.0+=dx;total.1+=dy;}
            other=>left.push(other),
        }
    }
    (total,left)
}
fn verify(key:&str,p:&Packet)->bool{let Ok(bytes)=serde_json::to_vec(&p.body)else{return false};let Ok(mut h)=HmacSha256::new_from_slice(key.as_bytes())else{return false};h.update(&bytes);h.verify_slice(&unhex(&p.mac)).is_ok()}
fn hex(bytes:&[u8])->String{bytes.iter().map(|b|format!("{b:02x}")).collect()}
fn unhex(s:&str)->Vec<u8>{s.as_bytes().chunks_exact(2).filter_map(|p|u8::from_str_radix(std::str::from_utf8(p).ok()?,16).ok()).collect()}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{input::Edge, protocol::InputEvent};

    fn packet(key:&str,body:Message)->Packet{
        let bytes=serde_json::to_vec(&body).unwrap();
        let mut h=HmacSha256::new_from_slice(key.as_bytes()).unwrap();
        h.update(&bytes);
        Packet{mac:hex(&h.finalize().into_bytes()),body}
    }

    #[test]
    fn session_messages_round_trip_through_the_envelope() {
        let key="a".repeat(64);
        for body in [Message::Input{event:InputEvent::Move{dx:-3.5,dy:125.25}},Message::Input{event:InputEvent::Key{code:"KeyQ".into(),down:true}},Message::Input{event:InputEvent::Wheel{dx:0,dy:-1}},Message::Clipboard{text:"中文 🧪".into()},Message::ReturnControl,Message::Control{enabled:false},Message::Disconnect]{
            let text=serde_json::to_string(&packet(&key,body)).unwrap();
            let parsed:Packet=serde_json::from_str(&text).unwrap();
            assert!(verify(&key,&parsed),"rejected: {text}");
        }
    }

    #[test]
    fn a_tampered_body_or_mac_is_rejected() {
        let key="a".repeat(64);
        let good=packet(&key,Message::Clipboard{text:"secret".into()});
        let mut tampered=good.clone();
        tampered.body=Message::Clipboard{text:"other".into()};
        assert!(!verify(&key,&tampered));
        let mut wrong_mac=good.clone();wrong_mac.mac=hex(b"deadbeef");
        assert!(!verify(&key,&wrong_mac));
        let mut truncated=good.clone();truncated.mac.truncate(63);
        assert!(!verify(&key,&truncated));
        assert!(!verify("b".repeat(64).as_str(),&good),"another session key must not verify");
    }

    #[test]
    fn a_burst_of_moves_becomes_one_injection_and_keeps_later_messages_in_order() {
        let move_by=|dx:f64,dy:f64|Frame::Body(Message::Input{event:InputEvent::Move{dx,dy}});
        let (total,left)=coalesce_moves((1.0,2.0),vec![move_by(3.0,4.0),move_by(-1.0,0.5)]);
        assert_eq!(total,(3.0,6.5));
        assert!(left.is_empty());
        let control=Frame::Body(Message::Control{enabled:true});
        let (total,left)=coalesce_moves((1.0,1.0),vec![move_by(2.0,2.0),control.clone(),move_by(9.0,9.0)]);
        assert_eq!(total,(3.0,3.0),"only the moves before the first non-move may fold");
        assert_eq!(left,vec![control,move_by(9.0,9.0)],"nothing after a non-move may be dropped");
        assert_eq!(coalesce_moves((5.0,-5.0),vec![]),((5.0,-5.0),vec![]));
    }

    #[tokio::test]
    async fn the_receive_path_skips_unverified_frames_and_ends_with_the_link() {
        use tokio_tungstenite::tungstenite::protocol::Role;
        async fn send_body<S>(peer:&mut WebSocketStream<S>,key:&str,m:Message)
        where S:tokio::io::AsyncRead+tokio::io::AsyncWrite+Unpin {
            peer.send(WsMessage::Text(serde_json::to_string(&packet(key,m)).unwrap().into())).await.unwrap();
        }
        let key="a".repeat(64);
        let (peer_side,our_side)=tokio::io::duplex(8192);
        let mut peer=WebSocketStream::from_raw_socket(peer_side,Role::Server,None).await;
        let ours=WebSocketStream::from_raw_socket(our_side,Role::Client,None).await;
        let (_sink,mut stream)=ours.split();
        send_body(&mut peer,&key,Message::Input{event:InputEvent::Move{dx:1.0,dy:2.0}}).await;
        peer.send(WsMessage::Binary(vec![1,2,3].into())).await.unwrap();
        let forged=Packet{mac:hex(b"deadbeef"),body:Message::Input{event:InputEvent::Move{dx:999.0,dy:999.0}}};
        peer.send(WsMessage::Text(serde_json::to_string(&forged).unwrap().into())).await.unwrap();
        send_body(&mut peer,&key,Message::Clipboard{text:"after the forged frame".into()}).await;
        // A signed binary chunk must survive; a tampered one must be dropped like a bad text MAC.
        let payload=chunk_payload(CHUNK_FILE,7,b"raw bytes");
        let mut bin=mac_bytes(&key,&payload);bin.extend_from_slice(&payload);
        peer.send(WsMessage::Binary(bin.clone().into())).await.unwrap();
        let mut forged_bin=bin.clone();forged_bin[40]^=0xff;
        peer.send(WsMessage::Binary(forged_bin.into())).await.unwrap();
        assert_eq!(next_body(&mut stream,&key).await,Some(Frame::Body(Message::Input{event:InputEvent::Move{dx:1.0,dy:2.0}})));
        assert_eq!(next_body(&mut stream,&key).await,Some(Frame::Body(Message::Clipboard{text:"after the forged frame".into()})));
        assert_eq!(next_body(&mut stream,&key).await,Some(Frame::Chunk{kind:CHUNK_FILE,transfer:7,data:b"raw bytes".to_vec()}));
        drop(peer);
        assert_eq!(next_body(&mut stream,&key).await,None,"a closed link must end the receive loop");
    }

    #[test]
    fn binary_chunks_are_authenticated_and_self_describing() {
        let key="a".repeat(64);
        let payload=chunk_payload(CHUNK_IMAGE,42,b"png");
        let mut bin=mac_bytes(&key,&payload);bin.extend_from_slice(&payload);
        assert_eq!(verify_chunk(&key,&bin),Some(Frame::Chunk{kind:CHUNK_IMAGE,transfer:42,data:b"png".to_vec()}));
        assert_eq!(verify_chunk("b".repeat(64).as_str(),&bin),None,"another key must not verify");
        let mut short=bin.clone();short.truncate(40);
        assert_eq!(verify_chunk(&key,&short),None,"a truncated header must be rejected");
        let mut tampered=bin.clone();tampered[35]^=1;
        assert_eq!(verify_chunk(&key,&tampered),None,"a flipped payload byte must fail the MAC");
    }

    #[test]
    fn pairing_enums_use_snake_case_tags() {
        assert_eq!(serde_json::to_string(&Message::Pair{pin:"123456".into(),label:"Mac".into(),token:None}).unwrap(),r#"{"type":"pair","pin":"123456","label":"Mac"}"#);
        assert_eq!(serde_json::to_string(&Message::PairResult{ok:true,key:Some("k".into()),label:None,trust:None}).unwrap(),r#"{"type":"pair_result","ok":true,"key":"k","label":null}"#);
    }

    #[test]
    fn pairing_codes_are_six_digits() {
        assert!(valid_pin("000000")&&valid_pin(&make_pin()));
        for bad in ["","12345","1234567","abcdef","12 456"] { assert!(!valid_pin(bad),"{bad} accepted"); }
    }

    #[test]
    fn only_unfixable_handshakes_end_the_retry_loop() {
        for e in ["配对码错误或已刷新","对端配对响应格式错误，双方需使用同一版本","对端没有提供会话密钥"] { assert!(fatal_pairing(e),"{e} should stop retrying"); }
        for e in ["Connection refused (os error 61)","timed out reading from transport","对端未返回配对结果"] { assert!(!fatal_pairing(e),"{e} should keep retrying"); }
    }

    #[test]
    fn hex_is_lossless_for_even_length_and_drops_garbage() {
        let bytes:[u8;32]=std::array::from_fn(|i| i as u8);
        assert_eq!(unhex(&hex(&bytes)).as_slice(),&bytes[..]);
        assert_eq!(unhex("zz"),Vec::<u8>::new());
        assert_eq!(unhex("abc"),vec![0xab]);
    }

    #[test]
    fn settings_survive_a_restart_and_reject_bad_edges() {
        let dir=std::env::temp_dir().join(format!("mouseborder-settings-{}",std::process::id()));
        let path=dir.join("settings.json");
        let _=std::fs::remove_dir_all(&dir);
        assert_eq!(read_settings(&path),None,"a missing file must fall back to defaults");
        let s=Settings{edge:"left".into(),edge_enabled:false,clipboard_enabled:true,devices:vec![Device{host:"192.168.1.25".into(),label:"PC".into(),token:"t".into()}]};
        write_settings(&path,&s);
        assert_eq!(read_settings(&path).as_ref(),Some(&s));
        for edge in ["top","bottom"] {
            write_settings(&path,&Settings{edge:edge.into(),edge_enabled:true,clipboard_enabled:true,devices:Vec::new()});
            assert_eq!(read_settings(&path).unwrap().edge,edge,"{edge} is a valid side and must survive");
        }
        write_settings(&path,&Settings{edge:"diagonal".into(),edge_enabled:true,clipboard_enabled:true,devices:Vec::new()});
        assert_eq!(read_settings(&path).unwrap().edge,"right");
        // A settings file written before devices existed must still load.
        std::fs::write(&path,br#"{"edge":"left","edge_enabled":true,"clipboard_enabled":true}"#).unwrap();
        let old=read_settings(&path).unwrap();
        assert_eq!(old.edge,"left");
        assert!(old.devices.is_empty());
        std::fs::write(&path,b"not json").unwrap();
        assert_eq!(read_settings(&path),None);
        let _=std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_side_moves_the_hook_and_unknown_sides_are_ignored() {
        let core=Core::new();
        for (name,want) in [("right",Edge::Right),("left",Edge::Left),("top",Edge::Top),("bottom",Edge::Bottom)] {
            core.set_edge(name.into());
            assert_eq!(core.hook.edge(),want,"{name} must reach the hook");
            assert_eq!(core.ui.lock().unwrap().edge,name,"{name} must reach the interface");
        }
        core.set_edge("diagonal".into());
        assert_eq!(core.hook.edge(),Edge::Bottom,"a rejected side must leave the hook where it was");
    }

    #[test]
    fn hook_failures_name_the_permission_and_the_retry() {
        let tap=hook_hint("global input hook failed: EventTapError");
        assert!(tap.starts_with(HOOK_ERROR_PREFIX),"{tap}");
        assert!(tap.contains("辅助功能")&&tap.contains("输入监控"),"{tap}");
        assert!(tap.contains("重试"),"{tap}");
        assert!(tap.contains("EventTapError"),"the raw library error stays diagnosable");
        assert!(hook_hint("PermissionDenied").starts_with(HOOK_ERROR_PREFIX));
    }

    #[test]
    fn a_retry_is_ignored_while_a_hook_is_already_live() {
        let core=Core::new();
        assert_eq!(core.hook_state.load(Ordering::SeqCst),HOOK_STOPPED);
        core.hook_state.store(HOOK_RUNNING,Ordering::SeqCst);
        core.retry_hook();
        assert_eq!(core.hook_state.load(Ordering::SeqCst),HOOK_RUNNING,"retry must not stack a second tap");
    }

    #[test]
    fn screen_geometry_feeds_the_handoff_edge_and_anchor() {
        let hook=HookState::new();
        hook.set_screen(0,0,1920,1080);
        assert!(input::entry_crossed(1918.0,0.0,1920.0,false));
        assert_eq!(hook.anchor(),(1559.0,540.0));
        hook.set_edge(Edge::Left);
        assert_eq!(hook.anchor(),(360.0,540.0));
        hook.set_edge(Edge::Bottom);
        assert_eq!(hook.anchor(),(960.0,719.0));
        hook.set_edge(Edge::Top);
        assert_eq!(hook.anchor(),(960.0,360.0));
        hook.set_edge(Edge::Right);
        hook.set_screen(-1920,-400,3840,1480);
        assert_eq!(hook.rect(),input::Rect{x:-1920.0,y:-400.0,w:3840.0,h:1480.0});
        assert_eq!(hook.anchor(),(1559.0,340.0));
        assert_eq!(hook.edge().coord(10.0,20.0),10.0);
        hook.set_edge(Edge::Top);
        assert_eq!(hook.edge().coord(10.0,20.0),20.0);
    }

    #[test]
    fn the_desktop_is_the_union_of_every_display() {
        // One screen: the union is that screen.
        assert_eq!(union_rect([(0,0,1920,1080)].into_iter()),(0,0,1920,1080));
        // A second screen to the right extends the desktop instead of replacing it.
        assert_eq!(union_rect([(0,0,1920,1080),(1920,0,3840,1080)].into_iter()),(0,0,3840,1080));
        // A screen left of and above the primary keeps its negative origin.
        assert_eq!(union_rect([(0,0,1920,1080),(-1440,-900,0,180)].into_iter()),(-1440,-900,3360,1980));
        // Order must not matter, and a mirror of the primary must not shrink the desktop.
        assert_eq!(union_rect([(-1440,-900,0,180),(0,0,1920,1080),(0,0,1920,1080)].into_iter()),(-1440,-900,3360,1980));
        assert_eq!(union_rect([].into_iter()),(0,0,1920,1080),"no display info must still leave a usable desktop");
    }

    #[test]
    fn targets_are_compared_by_host_and_code() {
        let core=Core::new();
        let a=Target{host:"192.168.1.25".into(),pin:"123456".into(),token:None};
        assert!(stale(&core,0,&a),"with nothing dialled no attempt owns the link");
        *core.target.lock().unwrap()=Some(a.clone());
        assert!(!stale(&core,0,&a));
        assert!(stale(&core,1,&a),"a superseding connect must stop the older attempt");
        assert!(stale(&core,0,&Target{host:a.host.clone(),pin:"654321".into(),token:None}),"a refreshed code must stop the old attempt");
        core.disconnect();
        assert!(stale(&core,0,&a),"disconnecting clears the dial target");
    }

    #[test]
    fn a_peer_disconnect_ends_the_retry_loop_without_touching_a_newer_one() {
        let core=Core::new();
        let target=Target{host:"192.168.1.25".into(),pin:String::new(),token:Some("t".into())};
        *core.target.lock().unwrap()=Some(target.clone());
        let gen=core.link.load(Ordering::SeqCst);
        assert!(!stale(&core,gen,&target));
        core.stop_retrying(gen);
        assert!(stale(&core,gen,&target),"the dialer must not redial after the peer hung up");
        assert!(core.target.lock().unwrap().is_none());
        assert_eq!(core.ui.lock().unwrap().notice,"对端已断开连接");

        // A connect the user issued while the old session was tearing down owns a newer generation.
        let newer=Target{host:"192.168.1.30".into(),pin:"123456".into(),token:None};
        *core.target.lock().unwrap()=Some(newer.clone());
        let current=core.link.load(Ordering::SeqCst);
        core.stop_retrying(gen);
        assert_eq!(core.link.load(Ordering::SeqCst),current,"a stale cleanup must not bump the link");
        assert_eq!(core.target.lock().unwrap().as_ref(),Some(&newer),"the newer target must survive");
    }

    #[test]
    fn a_remembered_device_can_be_dialled_and_forgotten() {
        let core=Core::new();
        core.remember("192.168.1.25".into(),"PC".into(),"token-a".into());
        assert_eq!(core.token_for("192.168.1.25").as_deref(),Some("token-a"));
        assert_eq!(core.ui.lock().unwrap().devices.len(),1);
        // Re-pairing the same host updates the record instead of stacking a duplicate.
        core.remember("192.168.1.25".into(),"PC-2".into(),"token-b".into());
        let devices=core.ui.lock().unwrap().devices.clone();
        assert_eq!(devices.len(),1);
        assert_eq!(devices[0].label,"PC-2");
        assert_eq!(core.token_for("192.168.1.25").as_deref(),Some("token-b"));
        // An empty label falls back to the address so the row is never blank.
        core.remember("192.168.1.30".into(),"  ".into(),"token-c".into());
        assert_eq!(core.ui.lock().unwrap().devices[1].label,"192.168.1.30");
        core.forget_device("192.168.1.25".into());
        assert_eq!(core.token_for("192.168.1.25"),None,"forgetting must revoke the stored token");
        assert_eq!(core.ui.lock().unwrap().devices.len(),1);
        core.connect_device("192.168.1.25".into());
        assert!(core.ui.lock().unwrap().error.contains("授权记录"),"a forgotten device must not dial");
    }

    #[test]
    fn persisted_settings_gate_the_live_flags() {
        let core=Core::new();
        core.apply_settings(&Settings{edge:"left".into(),edge_enabled:false,clipboard_enabled:false,devices:Vec::new()});
        assert!(!core.edge_enabled.load(Ordering::SeqCst));
        assert!(!core.clipboard_enabled.load(Ordering::SeqCst));
        assert_eq!(core.hook.edge(),Edge::Left);
        assert!(!core.hook.enabled.load(Ordering::SeqCst),"disconnected means the hook stays off");
        core.set_edge_enabled(true);
        let s=core.ui.lock().unwrap().clone();
        assert!(s.edge_enabled);
    }
}
