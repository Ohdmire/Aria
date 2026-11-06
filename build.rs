fn main() {
    tauri_build::build();

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
        .shared_flag(false)
        // Linux 下 Rust 默认以 PIE 链接(x86_64),目标文件必须带 -fPIC,
        // 否则部分重定位在链接期报 R_X86_64_32S / 只读段重定位。
        .pic(!cfg!(windows))
        .compile("soundtouch");
    if cfg!(target_os = "linux") {
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }
}
