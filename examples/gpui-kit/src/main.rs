//! The GPUI Kit demo on GPUI Fast with the gpui-mcp bridge installed: the
//! application the server's end-to-end tests drive.
#![expect(
    clippy::print_stderr,
    reason = "a demo that cannot start says why on stderr before it exits"
)]

use gpui_kit::{AppContext, WindowBounds, WindowOptions, assets::Assets, px, size};
use gpui_mcp::{AppId, BridgeConfig, BridgeHandle};
use gpui_mcp_kit_demo::{Demo, TITLE};

fn endpoint_dir() -> Option<std::path::PathBuf> {
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--endpoint-dir" {
            return arguments.next().map(std::path::PathBuf::from);
        }
    }
    None
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        gpui_kit::init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(640.0), px(720.0)), cx)),
            ..WindowOptions::default()
        };
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            window.set_window_title(TITLE);
            let app_id = AppId::new("gpui-kit-demo").unwrap_or_else(|error| {
                eprintln!("invalid app ID: {error}");
                std::process::exit(1);
            });
            let mut config = BridgeConfig::new(app_id, TITLE);
            if let Some(directory) = endpoint_dir() {
                config = config.endpoint_dir(directory).unwrap_or_else(|error| {
                    eprintln!("invalid endpoint directory: {error}");
                    std::process::exit(1);
                });
            }
            let bridge = BridgeHandle::install(window, cx, config).unwrap_or_else(|error| {
                eprintln!("could not install GPUI MCP bridge: {error}");
                std::process::exit(1);
            });
            cx.new(|cx| Demo::new(window, cx, Some(bridge)))
        });
        if let Err(error) = opened {
            eprintln!("could not open demo window: {error}");
            cx.quit();
            return;
        }
        cx.activate(true);
    });
}
