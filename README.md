SeaNVim
===

A neo vim plugin messing around with the luajit ffi rust bindings in oxy. 
In this plugin I demonstrate how to use libuv provided by luajit ffi.
I wrap the callbacks in futures, allowing async rust operations.
I wrote a little executor that leverages the nvim lua loop.
I even pulled in libuv-sys and the rust wrapper libuv and wrapped a tcp call in rust async.

This basically lets you write rust async functions backed by libuv via neovim.
Rust has a built in executor that runs on the libuv event loop.
It will use futures and wakers that leverage the libuv callbacks.

Additionally I brought in some high level rust libraries for TCP/HTTP.
They run on top of async-read and async-write traits that I implemented.

Setup
---

You'll need a version of nvim with luajit.
You might also want some sort of plugin manager, I'm using lazy with nvchad.

1. Check out this project
2. Run `cargo build`
3. Run `mkdir -p lua`
4. Run `mv target/debug/libseanvim.so lua/seanvim.so`
5. Get nvim to recognize the directory as a plugin. I use lazy so in ~/.config/nvim/lua/plugins/init.lua I added:

    {
        dir = "~/Documents/Workspace/blended/seanvim",
        opts = {
          url = "192.168.0.10:11434"
        },
        config = true,
        cmd = "SeanSpawn",
        keys = {
          {
            "<leader>sv",
            function()
              require("seanvim").ollama_generate()
            end,
            desc = "Update your buffer based on ollama response",
          },
        },
    },

6. Now in nvim run `:SeanSpawn yourname`
7. View the messages `:messages`
8. Or navigate somewhere in your buffer, and press space - s - v
9. This will send a system prompt and 15 lines above your cursor to Ollama generate.
10. It will then insert the response back into your buffer

Links and refs
---

(Lib UV Docs)[https://docs.libuv.org/en/v1.x/handle.html#c.uv_alloc_cb]
(Nvim OXI Github)[https://github.com/noib3/nvim-oxi/]
(Nvim OXI Docs)[https://docs.rs/nvim-oxi/latest/nvim_oxi/struct.Object.html#impl-From%3CString%3E-for-Object]
(LibUV Sys Rust Crate)[https://github.com/bmatcuk/libuv-sys/]
(LibUV Lua Bindings)[https://github.com/luvit/luv/tree/master]
(LuaJIT FFI)[https://luajit.org/ext_ffi.html]
(HTTP Types Rust Library)[https://github.com/http-rs/http-types]
(Http types docs)[https://docs.rs/http-types/latest/http_types/struct.Request.html#impl-AsyncRead-for-Request]
(Http Client Rust Github)[https://github.com/http-rs/http-client]
(Ollama API Refrence)[https://ollama.readthedocs.io/en/api/#parameters]
(NeoVim API Docs)[https://neovim.io/doc/user/builtin.html#getpos()]
