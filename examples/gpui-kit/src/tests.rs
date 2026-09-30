use super::Demo;
use gpui_kit::{
    AppContext, Bounds, Focusable, TestAppContext, WindowBounds, WindowOptions, px, size,
    test::TestWindowExt,
};
use gpui_mcp::{Automation, NodeAction, Role};

#[gpui_kit::test]
fn observes_and_drives_kit_controls(cx: &mut TestAppContext) {
    let automation = Automation::isolated();
    let observed = automation.clone();
    let (handle, view) = cx.update(|cx| {
        gpui_kit::init(cx);
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(640.0), px(420.0)),
                ))),
                ..WindowOptions::default()
            },
            cx,
            |window, cx| {
                observed.attach(window);
                cx.new(|cx| Demo::new(window, cx, None))
            },
        )
        .expect("open Kit test window")
    });
    cx.run_until_parked();

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let before = automation.snapshot();
        assert!(
            before.nodes.contains_key("increment"),
            "observed tree: {before:#?}"
        );
        assert_eq!(before.nodes["increment"].role, Role::Button);
        assert_eq!(
            before.nodes["increment"].label.as_deref(),
            Some("Increment")
        );
        assert!(
            before.nodes["increment"]
                .actions
                .contains(&NodeAction::Click)
        );
        assert_eq!(before.nodes["agree"].role, Role::Checkbox);
        assert_eq!(before.nodes["agree"].state.checked, Some(false));

        window.click("increment", cx);
        window.click("agree", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).count, 1);
        assert_eq!(
            automation.snapshot().nodes["agree"].state.checked,
            Some(true)
        );
        assert!(automation.frame_stats().frame_count > 0);
    })
    .expect("drive Kit controls");

    cx.update_window(handle, |_, window, cx| {
        // Kit exposes the outer frame as the semantic input and redirects its
        // accessibility Focus action to the inner editor's input handler.
        let input = automation
            .snapshot()
            .nodes
            .values()
            .find(|node| node.role == Role::TextInput)
            .expect("observed Kit text input")
            .id
            .clone();
        assert!(window.focus_observed_element(&input, cx));
        assert!(
            view.read(cx).input.focus_handle(cx).is_focused(window),
            "observed input {input}: {:?}",
            automation.snapshot()
        );
        window.render_frame(cx);
    })
    .expect("focus observed Kit input");
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        // These are the two GPUI hooks used by MCP's set_text and type_text.
        // Non-ASCII text exercises the handler's UTF-16 document ranges.
        assert!(
            view.read(cx).input.focus_handle(cx).is_focused(window),
            "Kit editor lost focus"
        );
        assert!(window.replace_input_text("Grüße 🦀", cx));
        assert!(window.insert_input_text("!", cx));
        window.render_frame(cx);
        assert_eq!(view.read(cx).input.read(cx).value(), "Grüße 🦀!");
        let tree = automation.snapshot();
        let input = tree
            .nodes
            .values()
            .find(|node| node.role == Role::TextInput)
            .expect("observed updated Kit input");
        assert_eq!(input.text.as_ref().expect("input text").text, "Grüße 🦀!");
    })
    .expect("replace and type Kit text");
}

#[gpui_kit::test]
fn wrapped_controls_keep_provenance_and_redaction(cx: &mut TestAppContext) {
    use gpui_kit::{Context, IntoElement, Render, TestSupportExt, Window, div, prelude::*, rgb};

    struct WrappedControl;
    impl Render for WrappedControl {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("wrapped-input")
                .role(gpui_kit::Role::TextInput)
                .aria_value("secret")
                .frame_redacted(true)
                .frame_metadata("wrapped", "true")
                .hover(|style| style.bg(rgb(0x112233)))
                .child("secret")
                .test_support()
        }
    }

    let automation = Automation::isolated();
    let observed = automation.clone();
    let (_, visual) = cx.add_window_view(|window, _| {
        observed.attach(window);
        WrappedControl
    });
    visual.run_until_parked();
    let tree = automation.snapshot();
    let node = &tree.nodes["wrapped-input"];
    assert_eq!(node.role, Role::TextInput);
    assert!(node.actions.contains(&NodeAction::Hover));
    assert_eq!(
        node.metadata.get("wrapped").map(String::as_str),
        Some("true")
    );
    assert!(node.text.as_ref().expect("redacted text").redacted);
    assert!(node.text.as_ref().expect("redacted text").text.is_empty());
    assert!(
        node.value
            .as_ref()
            .expect("redacted value")
            .value
            .is_empty()
    );
    assert_eq!(node.label, None);
}
