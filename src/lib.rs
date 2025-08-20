use std::cell::RefCell;
use std::mem;
use std::net::SocketAddr;
use std::rc::{Rc, Weak};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use futures::stream::FuturesUnordered;
use futures::task::{LocalFutureObj, LocalSpawn, LocalSpawnExt, SpawnError};
use futures::{Stream, StreamExt};
use libuv::{
    AsyncHandle, Buf, ConnectCB, Loop, ReadCB, ReadonlyBuf, StreamTrait, TcpHandle, WriteCB,
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

pub struct LocalSpawner {
    incoming: Weak<Incoming>,
    waker: Option<Waker>,
}

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

//Let's make an http call
struct MyTcp {
    handle: TcpHandle,
}

impl MyTcp {
    fn new(l: &Loop) -> Self {
        let tcp_h: TcpHandle = TcpHandle::new(l).unwrap();
        Self { handle: tcp_h }
    }

    pub async fn connect(&mut self, addr: SocketAddr) -> Result<u32, libuv::Error> {
        let ccb = ConnectFuture::new();
        let r = self.handle.connect(&addr, &ccb);
        if let Err(_e) = r {
            return Err(libuv::Error::EFAULT);
        }
        ccb.await
    }

    async fn write(&mut self, s: String) -> Result<u32, libuv::Error> {
        let wcb = WriteFuture::new();
        let bufs = Buf::new(&s).unwrap();
        let r = self.handle.write(&[bufs], &wcb);
        if let Err(_e) = r {
            return Err(libuv::Error::EFAULT);
        }
        wcb.await
    }

    fn read_start(&mut self) -> Result<Box<ReadStream>, libuv::Error> {
        print!("READSTART: 1");
        let rs = Box::new(ReadStream::new());
        let r = self.handle.read_start(
            |_handle, size| {
                print!("READSTART: 1A");
                Some(Buf::with_capacity(size).unwrap())
            },
            rs.as_ref(),
        );
        if let Err(e) = r {
            return Err(e);
        }
        Ok(rs)
    }

    fn read_stop(&mut self) -> Result<(), libuv::Error> {
        self.handle.read_stop()
    }

    /*
    fn close_reset(&mut self){
        self.handle.close()
    }
    */
}

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

struct WriteFuture {
    waker: Rc<RefCell<Option<Waker>>>,
    result: Rc<RefCell<Option<Result<u32, libuv::Error>>>>,
}

impl WriteFuture {
    fn new() -> Self {
        Self {
            waker: Rc::new(RefCell::new(None)),
            result: Rc::new(RefCell::new(None)),
        }
    }
}

impl Future for WriteFuture {
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

impl Into<WriteCB<'static>> for &WriteFuture {
    fn into(self) -> WriteCB<'static> {
        let iw = self.waker.clone();
        let ir = self.result.clone();
        WriteCB::CB(Box::new(move |_handle, status| {
            //I am finished, let's store the result back into myself and then call the waker
            ir.borrow_mut().replace(status);
            if let Some(ref w) = *iw.borrow() {
                w.wake_by_ref();
            }
        }))
    }
}

struct ReadStream {
    waker: Rc<RefCell<Option<Waker>>>,
    result: Rc<RefCell<Option<Result<(usize, ReadonlyBuf), libuv::Error>>>>,
}

impl ReadStream {
    fn new() -> Self {
        Self {
            waker: Rc::new(RefCell::new(None)),
            result: Rc::new(RefCell::new(None)),
        }
    }
}

impl Stream for ReadStream {
    type Item = Result<(usize, ReadonlyBuf), libuv::Error>;
    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        //The callback should transfer the result (inputs for the cb) to this future and then call
        //wake
        print!("READSTART: POLL: 1");
        match self.result.borrow_mut().take() {
            Some(r) => {
                print!("READSTART: POLL: Some: 1");
                if r.is_err() {
                    return Poll::Ready(None);
                }
                print!("READSTART: POLL: Some: 2");
                Poll::Ready(Some(r))
            }
            None => {
                print!("READSTART: POLL: None");
                self.waker.replace(Some(cx.waker().clone()));
                Poll::Pending
            }
        }
    }
}

impl Into<ReadCB<'static>> for &ReadStream {
    fn into(self) -> ReadCB<'static> {
        let iw = self.waker.clone();
        let ir = self.result.clone();
        ReadCB::CB(Box::new(move |_handle, status, buffer| {
            print!("READSTART: CB: 1");
            //I am finished, let's store the result back into myself and then call the waker
            if status.is_err() {
                return;
            }
            ir.borrow_mut().replace(Ok((status.unwrap(), buffer)));
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
    unsafe { mem::transmute(lua_loop) }
}

#[nvim_oxi::plugin]
pub fn seanvim() -> nvim_oxi::Result<Dictionary> {
    let greetings = |args: CommandArgs| {
        let who = args.args.unwrap_or("from  Rust".to_owned());
        let bang = if args.bang { "!" } else { "" };
        print!("Hello {}{}", who, bang);
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

    let _ = sean_vim.borrow().spawner().spawn_local(async move {
        let mut my_tcp = MyTcp::new(&l);
        let addr: SocketAddr = SocketAddr::from_str("192.168.254.37:11434").unwrap();
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
        let out = String::from(
            r#"GET / HTTP/1.1\r
Host: localhost\r
Connection: close\r\n\r\n"#,
        );
        let r = my_tcp.write(out).await;
        let _ = match r {
            Ok(r) => notify(&format!("Wrote! {}", r), LogLevel::Info, &Dictionary::new()),
            Err(e) => notify(&format!("Wrote? {}", e), LogLevel::Info, &Dictionary::new()),
        };
        use futures::StreamExt;
        let read_stream = my_tcp.read_start();
        if let Ok(mut read_stream) = read_stream {
            loop {
                let r = read_stream.next().await;
                if let Some(r) = r {
                    let _ = match r {
                        Ok(r) => notify(
                            &format!("Read! {}", r.0),
                            LogLevel::Info,
                            &Dictionary::new(),
                        ),
                        Err(e) => {
                            notify(&format!("Read? {}", e), LogLevel::Info, &Dictionary::new())
                        }
                    };
                }
            }
        }
    });

    Ok(api)
}
