extern crate winres;

fn main() {
    // Компилируем ресурсы только для Windows
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
        let mut res = winres::WindowsResource::new();
        // Указываем путь к файлу иконки
        res.set_icon("icon.ico");
        // Можно добавить и другие ресурсы Windows, если нужно (манифест и т.д.)
        // res.set_manifest_file("app.manifest");
        if let Err(e) = res.compile() {
            eprintln!("Failed to compile Windows resource file: {}", e);
            std::process::exit(1); // Падаем, если не удалось
        }
    }
}
