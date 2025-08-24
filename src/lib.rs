use std::cell::RefCell;
use std::collections::LinkedList;
use std::io::ErrorKind;
use std::mem;
use std::net::SocketAddr;
use std::rc::{Rc, Weak};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use futures::stream::FuturesUnordered;
use futures::task::{LocalFutureObj, LocalSpawn, LocalSpawnExt, SpawnError};
use futures::{AsyncRead, AsyncWrite, StreamExt};
use http_types::Request;
use http_types::convert::{Deserialize, Serialize, json};
use libuv::{
    AsyncHandle, Buf, BufTrait, ConnectCB, HandleTrait, Loop, ReadonlyBuf, StreamTrait, TcpHandle,
};
use libuv_sys2::uv_loop_t;
use nvim_oxi::api::{self, Window, notify, opts::*, types::*};
use nvim_oxi::lua::ffi::State;
use nvim_oxi::lua::with_state;
use nvim_oxi::{Dictionary, Function, Object, print, schedule};

type Incoming = RefCell<Vec<LocalFutureObj<'static, ()>>>;

#[derive(Default)]
struct SeanVim {
    url: Option<Object>,
    pool: FuturesUnordered<LocalFutureObj<'static, ()>>,
    incoming: Rc<Incoming>,
    waker: Option<Waker>,
}

#[derive(Clone)]
pub struct LocalSpawner {
    incoming: Weak<Incoming>,
    waker: Option<Waker>,
}

#[derive(Clone)]
pub struct LuvWaker {
    handle: Arc<Mutex<Option<AsyncHandle>>>,
}

impl LuvWaker {
    fn new() -> Self {
        Self {
            handle: Arc::new(Mutex::new(None)),
        }
    }
}

impl LocalSpawn for LocalSpawner {
    fn spawn_local_obj(&self, future: LocalFutureObj<'static, ()>) -> Result<(), SpawnError> {
        if let Some(incoming) = self.incoming.upgrade() {
            incoming.borrow_mut().push(future);
            if let Some(ref waker) = self.waker {
                waker.wake_by_ref();
            }
            Ok(())
        } else {
            Err(SpawnError::shutdown())
        }
    }

    fn status_local(&self) -> Result<(), SpawnError> {
        if self.incoming.upgrade().is_some() {
            Ok(())
        } else {
            Err(SpawnError::shutdown())
        }
    }
}

impl Wake for LuvWaker {
    fn wake(self: std::sync::Arc<Self>) {
        //I use myself as a waker
        //I will schedule a callback with libuv, where I will run through my pool
        //let _ = unsafe { self.handle.clone().assume_init().send() };
        let hc = self.handle.clone();
        if let Ok(mut o) = hc.lock() {
            if let Some(ref mut h) = *o {
                let _ = h.send();
            }
        }
    }
}

impl SeanVim {
    fn new() -> Self {
        Self {
            url: None,
            pool: FuturesUnordered::new(),
            incoming: Rc::new(RefCell::new(Vec::new())),
            waker: None,
        }
    }

    /// Get a clonable handle to the pool as a [`Spawn`].
    pub fn spawner(&self) -> LocalSpawner {
        LocalSpawner {
            incoming: Rc::downgrade(&self.incoming),
            waker: self.waker.clone(),
        }
    }

    fn setup(&mut self, config: Dictionary) {
        self.url = config.get("url").cloned();
    }

    fn run(&mut self) {
        // I need a waker here that can run this function in the future when any future gets its
        // wake called
        let Some(ref waker) = self.waker else {
            return;
        };
        let waker = waker.clone();
        let mut cx = Context::from_waker(&waker);
        loop {
            let ex_ret = loop {
                self.drain_incoming();
                // The pool tracks each future and task and ensures the task is polled
                // again when it's associated wake is called. I believe it will also
                // call the waker that I pass in as well. (Context)
                let pool_ret = self.pool.poll_next_unpin(&mut cx);
                if !self.incoming.borrow().is_empty() {
                    continue;
                }
                match pool_ret {
                    Poll::Ready(Some(())) => continue,
                    Poll::Ready(None) => break Poll::Ready(()),
                    Poll::Pending => break Poll::Pending,
                }
            };
            if let Poll::Ready(()) = ex_ret {
                break;
            }
            // We are stalled, we would typically park the thread or go to sleep
            if ex_ret.is_pending() {
                break;
            }
        }
    }

    fn drain_incoming(&mut self) {
        let mut incoming = self.incoming.borrow_mut();
        for task in incoming.drain(..) {
            self.pool.push(task);
        }
    }
}

type ReadTup = (usize, usize, ReadonlyBuf);

//Let's make an http call
#[derive(Clone)]
struct MyTcp {
    handle: TcpHandle,
    writes: Rc<RefCell<Vec<Option<Waker>>>>,
    read_start: Rc<RefCell<bool>>,
    read_waker: Rc<RefCell<Option<Waker>>>,
    reads: Rc<RefCell<LinkedList<Result<ReadTup, libuv::Error>>>>,
    close_waker: Rc<RefCell<Option<Waker>>>,
}

unsafe impl Sync for MyTcp {}

unsafe impl Send for MyTcp {}

impl MyTcp {
    fn new(l: &Loop) -> Self {
        let tcp_h: TcpHandle = TcpHandle::new(l).unwrap();
        Self {
            handle: tcp_h,
            writes: Rc::new(RefCell::new(Vec::new())),
            read_start: Rc::new(RefCell::new(false)),
            read_waker: Rc::new(RefCell::new(None)),
            reads: Rc::new(RefCell::new(LinkedList::new())),
            close_waker: Rc::new(RefCell::new(None)),
        }
    }

    pub async fn connect(&mut self, addr: SocketAddr) -> Result<u32, libuv::Error> {
        let ccb = ConnectFuture::new();
        let r = self.handle.connect(&addr, &ccb);
        if let Err(e) = r {
            eprintln!("ERRORS HAPPEN:connect: {}", e);
            return Err(libuv::Error::EFAULT);
        }
        ccb.await
    }
}

impl AsyncRead for MyTcp {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut_self = self.get_mut();
        let started_reading = *mut_self.read_start.borrow();
        if !started_reading {
            mut_self.read_start.replace(true);
            let ir = mut_self.reads.clone();
            let iw = mut_self.read_waker.clone();
            let r = mut_self.handle.read_start(
                Box::new(|_handle, size| {
                    let nb = Buf::with_capacity(size).unwrap();
                    eprintln!(
                        "ALLOC: size:{}, ptr: {:?}, base: {:?}, len: {:?}",
                        size,
                        nb.buf,
                        unsafe { *nb.buf }.base,
                        unsafe { *nb.buf }.len
                    );
                    Some(nb)
                }),
                Box::new(
                    move |_handle, status: Result<usize, libuv::Error>, buffer: ReadonlyBuf| {
                        match status {
                            Ok(nread) => {
                                //When this finishes the alloc'd buffer is gone and replaced with
                                //another alloc call. Need to copy it out right now
                                let buffer =
                                    Buf::new_from(&buffer, Some(nread)).unwrap().readonly();
                                eprintln!(
                                    "CB: ptr: {:?}, base: {:?}, len: {:?}",
                                    buffer.buf,
                                    unsafe { *buffer.buf }.base,
                                    unsafe { *buffer.buf }.len
                                );
                                //eprintln!( "CB: b.allocated: {}, size: {}, ptr: {:?}, contents: {:?}", buffer.is_allocated(), nread, buffer.index(0..6).as_ptr(), &buffer.index(0..6));
                                ir.borrow_mut().push_back(Ok((0, nread, buffer)));
                                if let Some(w) = iw.take() {
                                    w.wake();
                                }
                            }
                            Err(e) => {
                                ir.borrow_mut().push_back(Err(e));
                                if let Some(w) = iw.take() {
                                    w.wake();
                                }
                            }
                        };
                    },
                ),
            );
            if let Err(e) = r {
                eprintln!("ERRORS HAPPEN:read_start: {}", e);
                return Poll::Ready(Err(std::io::Error::other(e)));
            }
        }

        eprintln!("Read0");
        if mut_self.read_waker.borrow().is_some() {
            eprintln!("Read1");
            return Poll::Ready(Err(std::io::Error::from(ErrorKind::AlreadyExists)));
        }

        eprintln!("Read2");
        if mut_self.reads.borrow().is_empty() {
            eprintln!("Read3");
            mut_self.read_waker.borrow_mut().replace(cx.waker().clone());
            return Poll::Pending;
        }

        eprintln!("Read4");
        Poll::Ready(copy_into_from_vec_buffs(
            buf,
            &mut mut_self.reads.borrow_mut(),
        ))
    }
}

//Will consume from the vec of buffers into the into_buff until it is full or there is nothing left
fn copy_into_from_vec_buffs(
    into_buff: &mut [u8],
    from_buffs: &mut LinkedList<Result<(usize, usize, ReadonlyBuf), libuv::Error>>,
) -> Result<usize, std::io::Error> {
    let mut into_buff_bytes_read: usize = 0;

    //Check incoming
    match from_buffs.front_mut() {
        Some(Ok((start, len, b))) => {
            //eprintln!( "Copy1P: b.allocated: {}, size: {}, ptr: {:?}, contents: {:?}", b.is_allocated(), len, b.index(0..6).as_ptr(), &b.index(0..6));
            eprintln!(
                "Copy1P: size:{}, ptr: {:?}, base: {:?}, len: {:?}",
                len,
                b.buf,
                unsafe { *b.buf }.base,
                unsafe { *b.buf }.len
            );
            //eprintln!( "Copy1: br: {},{} f: {},{} contents: {:?}", into_buff_bytes_read, into_buff.len(), start, len, &b.index(0..6),);
            //If there is less (or equal) incoming than room in my buffer, consume it all and copy into incoming
            //and return bytes_consumed
            if (*len - *start) <= into_buff.len() - into_buff_bytes_read {
                eprintln!(
                    "Copy1C: ({},{}) -> ({},{})",
                    *start,
                    *len,
                    into_buff_bytes_read,
                    *len - *start,
                );
                for i in 0..into_buff.len() {
                    into_buff[i] = b[*start + i];
                }
                //into_buff[into_buff_bytes_read..*len - *start - 1]
                //    .copy_from_slice(&b[*start..*len - 1]);
                //eprintln!("Copy1V: {:?} == {:?}", &into_buff[0..6], &b.index(0..6));
                //eprintln!("Copy1P: bfptr: {:?}", b.buf);
                into_buff_bytes_read += *len - *start;
                //Nothing left in this buffer, discard
                from_buffs.pop_front();
            } else {
                //If there is not enough room in my buffer for the amount in the incoming then update that
                //to reflect how much I was able to read, leave it alone and then
                let into_len = into_buff.len();
                //TODO These buffers do not like the indexing math, so it might be easier to do it
                //manually, there might be a hidden issue here
                let nsb: &mut [u8] = &mut into_buff[into_buff_bytes_read..];

                nsb.copy_from_slice(&b[*start..(*start + into_len)]);
                into_buff_bytes_read += *start + into_len;
                *start += into_len;
            }
        }
        Some(Err(e)) => {
            eprintln!(
                "Copy2: br: {},{} e: {}",
                into_buff_bytes_read,
                into_buff.len(),
                e
            );
            if let libuv::EOF = e {
                //eprintln!("Returning: {}, underlying slice contains: {:?}", into_buff_bytes_read, &into_buff[0..into_buff_bytes_read]);
                return Ok(into_buff_bytes_read);
            }
            let ret = Err(std::io::Error::other(*e));
            //Consume the error off the buffer and return it
            from_buffs.pop_front();
            return ret;
        }
        _ => {}
    }

    eprintln!("Copy4: br: {},{}", into_buff_bytes_read, into_buff.len());
    //eprintln!("Copy4: first 6 bytes: {:?}", &into_buff[0..6]);
    Ok(into_buff_bytes_read)
}

// libuv is completion based io, whereas async write is (poll based io) not
// https://users.rust-lang.org/t/will-asyncwrite-give-the-same-buf-on-returning-poll-pending/104031/3
// This ensures that there is a queue of writes that occour
impl AsyncWrite for MyTcp {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        //Need to dump the bytes and return ready for this buffer
        eprintln!(
            "Writing {} bytes: {}",
            buf.len(),
            std::str::from_utf8(buf).unwrap()
        );
        let mut bufsn = Buf::new_from_bytes(buf).unwrap();
        let mut bufs = Buf::new_from(&bufsn, Some(buf.len())).unwrap();
        let writes = self.writes.clone();
        writes.borrow_mut().push(None);
        //set up a callback for flush to wait on if called
        let r = self
            .get_mut()
            .handle
            .write(&[bufs], move |_handle, _status| {
                bufsn.destroy();
                bufs.destroy();
                if let Some(Some(w)) = writes.borrow_mut().pop() {
                    w.wake_by_ref();
                }
            });
        match r {
            Ok(_r) => Poll::Ready(Ok(buf.len())),
            Err(e) => Poll::Ready(Err(std::io::Error::other(e))),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        eprintln!("FLUSHING!!!");
        if self.writes.borrow().is_empty() {
            return Poll::Ready(Ok(()));
        }

        //Set myself up as a waker on the bottom of the stack
        if let Some(i) = self.writes.borrow_mut().get_mut(0) {
            i.replace(cx.waker().clone());
        }
        Poll::Pending
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        eprintln!("Closing");
        if self.close_waker.borrow().is_some() {
            return Poll::Ready(Err(std::io::Error::from(ErrorKind::AlreadyExists)));
        }

        self.close_waker.replace(Some(cx.waker().clone()));
        let cw = self.close_waker.clone();
        self.get_mut().handle.close(move |_handle| {
            if let Some(ref w) = cw.borrow_mut().take() {
                w.wake_by_ref();
            }
        });
        Poll::Pending
    }
}

#[derive(Clone)]
struct ConnectFuture {
    waker: Rc<RefCell<Option<Waker>>>,
    result: Rc<RefCell<Option<Result<u32, libuv::Error>>>>,
}

impl ConnectFuture {
    fn new() -> Self {
        Self {
            waker: Rc::new(RefCell::new(None)),
            result: Rc::new(RefCell::new(None)),
        }
    }
}

impl Future for ConnectFuture {
    type Output = Result<u32, libuv::Error>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        //The callback should transfer the result (inputs for the cb) to this future and then call
        //wake
        match self.result.borrow_mut().take() {
            Some(r) => Poll::Ready(r),
            None => {
                self.waker.replace(Some(cx.waker().clone()));
                Poll::Pending
            }
        }
    }
}

#[allow(clippy::from_over_into)]
impl Into<ConnectCB<'static>> for &ConnectFuture {
    fn into(self) -> ConnectCB<'static> {
        let iw = self.waker.clone();
        let ir = self.result.clone();
        ConnectCB::CB(Box::new(move |_handle, result| {
            //I am finished, let's store the result back into myself and then call the waker
            ir.borrow_mut().replace(result);
            if let Some(ref w) = *iw.borrow() {
                w.wake_by_ref();
            }
        }))
    }
}

unsafe extern "C" {
    pub fn luv_loop(lua_state: *mut State) -> *mut uv_loop_t;
}

fn get_lua_loop() -> Loop {
    #![allow(dead_code)]
    struct LuaLoop {
        handle: *mut uv_loop_t,
        should_drop: bool,
    }
    let lua_loop: LuaLoop = LuaLoop {
        handle: unsafe { with_state(|state| luv_loop(state)) },
        should_drop: false,
    };
    eprintln!("LLPTR: {:?}", lua_loop.handle);
    unsafe { mem::transmute(lua_loop) }
}

#[nvim_oxi::plugin]
pub fn seanvim() -> nvim_oxi::Result<Dictionary> {
    let greetings = |args: CommandArgs| {
        let who = args.args.unwrap_or("from  Rust".to_owned());
        let bang = if args.bang { "!" } else { "" };
        eprintln!("Hello {}{}", who, bang);
    };

    api::create_user_command(
        "SeanVim",
        greetings,
        &CreateCommandOpts::builder()
            .bang(true)
            .desc("shows a greetings message")
            .nargs(CommandNArgs::ZeroOrOne)
            .build(),
    )?;

    //This is getting annoying
    //api::set_keymap(Mode::Insert, "hi", "hello", &Default::default())?;

    let sean_vim: Rc<RefCell<SeanVim>> = Rc::new(RefCell::new(SeanVim::new()));
    let uninit_waker = LuvWaker::new();
    let sv = sean_vim.clone();
    let mut_waker = Arc::new(uninit_waker);
    let waker = Waker::from(mut_waker.clone());
    let l = get_lua_loop();
    let waker_handle = AsyncHandle::new(&l, move |_| {
        //Schedule on the 'main' thread the async runtime
        let sv = sv.clone();
        schedule(move |()| {
            sv.borrow_mut().run();
        });
    })
    .unwrap();
    //Need to init the waker now that I have a handle
    mut_waker.handle.lock().unwrap().replace(waker_handle);
    //Set up my app to be able to clone the waker for runs
    sean_vim.borrow_mut().waker = Some(waker);

    // Creates two functions `{open,close}_window` to open and close a
    // floating window.
    let mut buf = api::create_buf(false, true)?;
    let win: Rc<RefCell<Option<Window>>> = Rc::default();
    let _ = buf.set_name("SeanVimBuff");

    let sv = sean_vim.clone();
    let setup: Function<Dictionary, Result<(), api::Error>> =
        Function::from_fn(move |opts: Dictionary| {
            let mut svi = sv.borrow_mut();
            svi.setup(opts);
            Ok(())
        });

    let sv = sean_vim.clone();
    api::create_user_command(
        "SeanSpawn",
        move |args: CommandArgs| {
            let msg = args.args.unwrap_or("from Rust Async".to_owned());
            let bang = if args.bang { "!" } else { "" };
            let _ = sv.borrow().spawner().spawn_local(async move {
                let _ = notify(
                    &format!("Hello from command spawned future{}{}", msg, bang),
                    LogLevel::Info,
                    &Dictionary::new(),
                );
            });
        },
        &CreateCommandOpts::builder()
            .bang(true)
            .desc("shows a greetings message")
            .nargs(CommandNArgs::ZeroOrOne)
            .build(),
    )?;

    let w = Rc::clone(&win);
    let b = buf.clone();
    let open_window: Function<(), Result<(), api::Error>> = Function::from_fn(move |()| {
        if w.borrow().is_some() {
            api::err_writeln("Window is already open");
            return Ok(());
        }

        let config = WindowConfig::builder()
            .relative(WindowRelativeTo::Cursor)
            .height(5)
            .width(50)
            .row(1)
            .col(0)
            .build();

        let mut win = w.borrow_mut();
        *win = Some(api::open_win(&b, false, &config)?);

        Ok(())
    });

    let w = Rc::clone(&win);
    let b = buf.clone();
    let sean_open = move |_args: CommandArgs| {
        if w.borrow().is_some() {
            api::err_writeln("Window is already open");
            return;
        }

        let config = WindowConfig::builder()
            .relative(WindowRelativeTo::Cursor)
            .height(5)
            .width(10)
            .row(1)
            .col(0)
            .build();

        let mut win = w.borrow_mut();
        *win = Some(api::open_win(&b, false, &config).unwrap());
    };

    api::create_user_command(
        "SeanOpen",
        sean_open,
        &CreateCommandOpts::builder()
            .bang(true)
            .desc("opens a window")
            .nargs(CommandNArgs::ZeroOrOne)
            .build(),
    )?;

    let w = Rc::clone(&win);
    let close_window: Function<(), Result<(), api::Error>> = Function::from_fn(move |()| {
        if w.borrow().is_none() {
            api::err_writeln("Window is already closed");
            return Ok(());
        }

        let win = w.borrow_mut().take().unwrap();
        win.close(false)
    });

    let w = Rc::clone(&win);
    let sean_close = move |_args: CommandArgs| {
        if w.borrow().is_none() {
            api::err_writeln("Window is already closed");
            return Ok(());
        }

        let win = w.borrow_mut().take().unwrap();
        win.close(false)
    };

    api::create_user_command(
        "SeanClose",
        sean_close,
        &CreateCommandOpts::builder()
            .bang(true)
            .desc("closes a window")
            .nargs(CommandNArgs::ZeroOrOne)
            .build(),
    )?;

    let mut api: Dictionary = Dictionary::new();
    api.insert("setup", setup);
    api.insert("open_window", open_window);
    api.insert("close_window", close_window);

    let _ = sean_vim.borrow().spawner().spawn_local(async {
        let _ = notify("Hello", LogLevel::Info, &Dictionary::new());
    });

    api::create_user_command(
        "SeanOllama",
        move |args: CommandArgs| {
            let msg = args.args.unwrap_or("can you introduce yourself?".to_owned());
            let _ = sean_vim.borrow().spawner().spawn_local(async move {
                let l = get_lua_loop();
                let mut my_tcp = MyTcp::new(&l);
                let addr: SocketAddr = SocketAddr::from_str("192.168.254.37:11434").unwrap();
                //let addr: SocketAddr = SocketAddr::from_str("104.248.184.153:80").unwrap();

                let r = my_tcp.connect(addr).await;
                let _ = match r {
                    Ok(r) => notify(
                        &format!("Connected! {}", r),
                        LogLevel::Info,
                        &Dictionary::new(),
                    ),
                    Err(e) => notify(
                        &format!("Connected? {}", e),
                        LogLevel::Info,
                        &Dictionary::new(),
                    ),
                };
                if r.is_err() {
                    return;
                }

                //TODO NEXT I'd like to get the selected text here and pass it to the llm with some context


                let mut my_req = Request::post("http://seanvim.requestcatcher.com/api/generate");
                my_req.insert_header("User-Agent", "nvim-libuv");
                my_req.insert_header("Accept", "application/json");
                my_req.insert_header("Content-Type", "application/json");
                my_req.insert_header("Connection", "close");
                let rb = json!({
                    "model": "gemma3:12b",
                    "prompt": "can you introduce yourself?",
                    "stream": false
                });
                my_req.set_body(rb);
                let r = async_h1::connect(my_tcp, my_req).await;
                match r {
                    Ok(mut res) => {
                        print!("Response: {:?}", res);
                        let b = res.take_body();
                        //let s = b.into_string().await.unwrap();
                        //print!("Response Body: {}", s);
                        let j_r = b.into_json::<OllamaGenerateResponse>().await;
                        print!("Response: {:?}", j_r);
                        if let Ok(j) = j_r {
                            print!("Response Body: {}", j.response);
                            let _ = nvim_oxi::api::call_function::<(&str, &str), ()>(
                                "setreg",
                                ("o", j.response.as_str()),
                            );
                        }
                    }
                    Err(e) => print!("Response: ERROR: {:?}", e),
                }
            });
        },
        &CreateCommandOpts::builder()
            .bang(true)
            .desc("dumps an ollama call into the o register")
            .nargs(CommandNArgs::ZeroOrOne)
            .build(),
    )?;

    Ok(api)
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaGenerateResponse {
    response: String,
}
