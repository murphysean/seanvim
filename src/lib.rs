use std::cell::RefCell;
use std::collections::LinkedList;
use std::io::ErrorKind;
use std::mem;
use std::net::SocketAddr;
use std::ops::{Index, IndexMut};
use std::rc::{Rc, Weak};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use futures::stream::FuturesUnordered;
use futures::task::{LocalFutureObj, LocalSpawn, LocalSpawnExt, SpawnError};
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, Stream, StreamExt, pin_mut};
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

//Let's make an http call
#[derive(Clone)]
struct MyTcp {
    handle: TcpHandle,
    writes: Rc<RefCell<Vec<Option<Waker>>>>,
    read_start: Rc<RefCell<bool>>,
    read_waker: Rc<RefCell<Option<Waker>>>,
    reads: Rc<RefCell<LinkedList<Result<(usize, usize, ReadonlyBuf), libuv::Error>>>>,
}

impl MyTcp {
    fn new(l: &Loop) -> Self {
        let tcp_h: TcpHandle = TcpHandle::new(l).unwrap();
        Self {
            handle: tcp_h,
            writes: Rc::new(RefCell::new(Vec::new())),
            read_start: Rc::new(RefCell::new(false)),
            read_waker: Rc::new(RefCell::new(None)),
            reads: Rc::new(RefCell::new(LinkedList::new())),
        }
    }

    pub async fn connect(&mut self, addr: SocketAddr) -> Result<u32, libuv::Error> {
        let ccb = ConnectFuture::new();
        let r = self.handle.connect(&addr, &ccb);
        if let Err(e) = r {
            print!("ERRORS HAPPEN:connect: {}", e);
            return Err(libuv::Error::EFAULT);
        }
        ccb.await
    }

    /*
    async fn write(&mut self, s: String) -> Result<u32, libuv::Error> {
        let wcb = WriteFuture::new();
        let bufs = Buf::new(&s).unwrap();
        let r = self.handle.write(&[bufs], &wcb);
        if let Err(e) = r {
            print!("ERRORS HAPPEN:write: {}", e);
            return Err(e);
        }
        wcb.await
    }

    fn read_start(&mut self) -> Result<Box<ReadStream>, libuv::Error> {
        let rs = Box::new(ReadStream::new());
        let r = self.handle.read_start(
            |_handle, size| Some(Buf::with_capacity(size).unwrap()),
            rs.as_ref(),
        );
        if let Err(e) = r {
            print!("ERRORS HAPPEN:read_start: {}", e);
            return Err(e);
        }
        Ok(rs)
    }
    */

    #[allow(dead_code)]
    fn read_stop(&mut self) -> Result<(), libuv::Error> {
        self.handle.read_stop()
    }

    /*
    fn close_reset(&mut self){
        self.handle.close()
    }
    */
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
                Box::new(|_handle, size| Some(Buf::with_capacity(size).unwrap())),
                Box::new(
                    move |_handle, status: Result<usize, libuv::Error>, buffer| {
                        match status {
                            Ok(nread) => {
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
                print!("ERRORS HAPPEN:read_start: {}", e);
                return Poll::Ready(Err(std::io::Error::other(e)));
            }
        }

        print!("HERE0");
        if mut_self.read_waker.borrow().is_some() {
            print!("HERE1");
            return Poll::Ready(Err(std::io::Error::from(ErrorKind::AlreadyExists)));
        }

        print!("HERE2");
        if mut_self.reads.borrow().is_empty() {
            print!("HERE3");
            mut_self.read_waker.borrow_mut().replace(cx.waker().clone());
            return Poll::Pending;
        }

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

    print!("MVNBF0: br: {},{}", into_buff_bytes_read, into_buff.len());
    loop {
        //Check incoming
        match from_buffs.front_mut() {
            Some(Ok((start, len, b))) => {
                print!("MVNBF1: br: {},{} f: {},{}", into_buff_bytes_read, into_buff.len(), start, len);
                //If there is less (or equal) incoming than room in my buffer, consume it all and copy into incoming
                //and return bytes_consumed
                if (*len - *start) <= into_buff.len() - into_buff_bytes_read {
                    let nsb: &mut [u8] = into_buff.index_mut(into_buff_bytes_read..*len - *start);
                    nsb.copy_from_slice(b.index(*start..*len));
                    into_buff_bytes_read += *len - *start;
                    from_buffs.pop_front();
                } else {
                    //If there is not enough room in my buffer for the amount in the incoming then update that
                    //to reflect how much I was able to read, leave it alone and then
                    let into_len = into_buff.len();
                    let nsb: &mut [u8] = into_buff.index_mut(into_buff_bytes_read..);
                    nsb.copy_from_slice(b.index(*start..(*start + into_len)));
                    into_buff_bytes_read += *start + into_len;
                    *start += into_len;
                }
            }
            Some(Err(e)) => {
                print!("MVNBF2: br: {},{} e: {}", into_buff_bytes_read, into_buff.len(), e);
                if let libuv::EOF = e {
                    return Ok(into_buff_bytes_read);
                }
                let ret =Err(std::io::Error::other(*e)); 
                //Consume the error off the buffer and return it
                from_buffs.pop_front();
                return ret;
            }
            _ => {}
        }
        print!("MVNBF3: br: {},{}", into_buff_bytes_read, into_buff.len());
        if into_buff_bytes_read >= into_buff.len() {
            break;
        }
    }

    print!("MVNBF4: br: {},{}", into_buff_bytes_read, into_buff.len());
    Ok(into_buff_bytes_read)
}

impl AsyncWrite for MyTcp {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        //Need to dump the bytes and return ready for this buffer
        let bufs = Buf::new_from_bytes(buf).unwrap();
        let writes = self.writes.clone();
        writes.borrow_mut().push(None);
        //set up a callback for flush to wait on if called
        let r = self
            .get_mut()
            .handle
            .write(&[bufs], move |_handle, _status| {
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
        todo!()
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

/*
#[derive(Clone)]
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

#[allow(clippy::from_over_into)]
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

#[derive(Clone)]
struct ReadStream {
    waker: Rc<RefCell<Option<Waker>>>,
    result: Rc<RefCell<Option<Result<String, libuv::Error>>>>,
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
    type Item = Result<String, libuv::Error>;
    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        //The callback should transfer the result  to this future and then call wake
        match self.result.borrow_mut().take() {
            Some(r) => {
                if r.is_err() {
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(r))
            }
            None => {
                self.waker.replace(Some(cx.waker().clone()));
                Poll::Pending
            }
        }
    }
}

#[allow(clippy::from_over_into)]
impl Into<ReadCB<'static>> for &ReadStream {
    fn into(self) -> ReadCB<'static> {
        let iw = self.waker.clone();
        let ir = self.result.clone();
        ReadCB::CB(Box::new(move |_handle, status, buffer| {
            //I am finished, let's store the result back into myself and then call the waker
            if status.is_err() {
                return;
            }
            //I think this buffer dissapears after I return right here
            let s = String::from(buffer.to_str(status.unwrap()).unwrap());
            ir.borrow_mut().replace(Ok(s));
            if let Some(ref w) = *iw.borrow() {
                w.wake_by_ref();
            }
        }))
    }
}
*/

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

    let mut b = buf.clone();
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
            "GET / HTTP/1.1\r\nHost: localhost\r\nUser-Agent: nvim-libuv\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        );
        let r = my_tcp.write(out.as_bytes()).await;
        let _ = match r {
            Ok(r) => notify(&format!("Wrote! {}", r), LogLevel::Info, &Dictionary::new()),
            Err(e) => notify(&format!("Wrote? {}", e), LogLevel::Info, &Dictionary::new()),
        };
        let mut in_vec: Vec<u8> = vec![0;2048];
        let r = my_tcp.read(&mut in_vec[..]).await;
        let _ = match r {
            Ok(r) => notify(&format!("Read! {}", r), LogLevel::Info, &Dictionary::new()),
            Err(e) => notify(&format!("Read? {}", e), LogLevel::Info, &Dictionary::new()),
        };
        //use futures::StreamExt;
        //let read_stream = my_tcp.read_start();
        /*
        if let Ok(read_stream) = read_stream {
            pin_mut!(read_stream);
            loop {
                let r = read_stream.next().await;
                if let Some(r) = r {
                    match r {
                        Ok(r) => {
                            let _ = b.set_lines(.., false, vec!(r));
                        },
                        Err(e) => {
                            let _ = notify(&format!("Read? {}", e), LogLevel::Info, &Dictionary::new());
                        }
                    };
                }
            }
        }else{
            print!("an error!");
        }
        */
    });

    Ok(api)
}
