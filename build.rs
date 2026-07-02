fn main() {
    // 翻訳(.po)をビルド時にバイナリへ埋め込む。ランタイムに .mo を配置する必要がない。
    let config = slint_build::CompilerConfiguration::new()
        .with_bundled_translations("translations/");
    slint_build::compile_with_config("ui/main.slint", config).unwrap();
}
