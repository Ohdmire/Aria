fn main() {
    // SoundTouch C++ 源码 + C 接口垫片,静态编译进本 crate。
    // (MSVC 的 C++ 符号修饰与 bindgen 预生成绑定不兼容,垫片导出纯 C
    // 符号,Rust 侧零 mangling 依赖。)
    let st = std::path::Path::new("native/soundtouch");
    let src = st.join("source/SoundTouch");
    let mut cc = cc::Build::new();
    cc.warnings(false)
        .cpp(true)
        .extra_warnings(false)
        .file("native/soundtouch_shim.cpp")
        .file(src.join("AAFilter.cpp"))
        .file(src.join("FIFOSampleBuffer.cpp"))
        .file(src.join("FIRFilter.cpp"))
        .file(src.join("InterpolateCubic.cpp"))
        .file(src.join("InterpolateLinear.cpp"))
        .file(src.join("InterpolateShannon.cpp"))
        .file(src.join("PeakFinder.cpp"))
        .file(src.join("RateTransposer.cpp"))
        .file(src.join("SoundTouch.cpp"))
        .file(src.join("TDStretch.cpp"))
        .file(src.join("cpu_detect_x86.cpp"))
        .file(src.join("mmx_optimized.cpp"))
        .file(src.join("sse_optimized.cpp"))
        .include(st.join("include"))
        .include(&src)
        // Linux 下 Rust 默认以 PIE 链接(x86_64),目标文件必须带 -fPIC,
        // 否则部分重定位在链接期报 R_X86_64_32S / 只读段重定位。
        .pic(!cfg!(windows))
        .compile("soundtouch");
    if cfg!(target_os = "linux") {
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }

    // Windows:把 icons/icon.ico 编进 exe(资源 id 1)。
    // GPUI 的 Windows 后端用 LoadImageW(模块句柄, id 1) 取窗口图标,任务栏 /
    // Alt-Tab 都用它;不嵌入就只剩系统默认的空白图标。编译失败(缺 rc.exe)
    // 只警告不中断构建 —— 图标是装饰,不该挡住编译。
    #[cfg(windows)]
    {
        let manifest = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
        let icon = manifest.join("icons").join("icon.ico");
        println!("cargo:rerun-if-changed=icons/icon.ico");
        let rc = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("aria.rc");
        // rc.exe 用 / 分隔也行,省得写转义的反斜杠
        let icon = icon.display().to_string().replace('\\', "/");
        if std::fs::write(&rc, format!("1 ICON \"{icon}\"\n")).is_ok() {
            embed_resource::compile(&rc, embed_resource::NONE)
                .manifest_optional()
                .unwrap_or_else(|e| println!("cargo:warning=嵌入图标失败(不影响运行): {e}"));
        }
    }
}
