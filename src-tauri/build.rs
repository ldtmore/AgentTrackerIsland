fn main() {
    // 图标文件变更必须触发构建脚本重跑：Windows 下窗口/任务栏/托盘的嵌入图标
    // 在编译期写入 exe 资源，默认增量编译不跟踪 icons 目录内容变化，
    // 不声明 rerun-if-changed 时替换图标后 exe 里仍是旧图（2026-09-17 实测踩坑）
    println!("cargo:rerun-if-changed=tauri.conf.json");
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=icons/icon.png");
    println!("cargo:rerun-if-changed=icons/32x32.png");
    println!("cargo:rerun-if-changed=icons/128x128.png");
    println!("cargo:rerun-if-changed=icons/128x128@2x.png");
    tauri_build::build()
}
