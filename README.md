SeaNVim
===

A neo vim plugin messing around with the luajit ffi rust bindings in oxy. 
In this plugin I demonstrate how to use libuv provided by luajit ffi.
I wrap the callbacks in futures, allowing async rust operations.
I wrote a little executor that leverages the nvim lua loop.
I even pulled in libuv-sys and the rust wrapper libuv and wrapped a tcp call in rust async.

I'm thinking it might be nice to have libuv provide future compat handles,
and an executor so you can write some nice async functions and tasks.

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
        config = true,
        cmd = "SeanVim"
    },

6. Now in nvim run `:SeanVim yourname`
7. View the messages `:messages`
