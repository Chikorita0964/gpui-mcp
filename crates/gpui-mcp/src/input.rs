use gpui::{
    App, FocusHandle, Keystroke, Modifiers, MouseButton as GpuiMouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PlatformInput, ScrollDelta, ScrollWheelEvent, TouchPhase, Window,
    point, px,
};
use gpui_mcp_protocol::{
    BridgeError, ErrorCode, InputCommand, MAX_KEY_SEQUENCE, MAX_TEXT_BYTES, MouseButton, Point,
    PointerCommand, PointerScrollDelta,
};

pub(crate) fn validate(command: &InputCommand) -> Result<(), BridgeError> {
    match command {
        InputCommand::Key { keystroke } => {
            validate_text(keystroke)?;
            Keystroke::parse(keystroke).map_err(|_| invalid("invalid GPUI keystroke syntax"))?;
        }
        InputCommand::KeySequence { keystrokes } => {
            if keystrokes.is_empty() || keystrokes.len() > MAX_KEY_SEQUENCE {
                return Err(invalid("key sequence must contain 1 through 1024 events"));
            }
            for keystroke in keystrokes {
                validate_text(keystroke)?;
                Keystroke::parse(keystroke)
                    .map_err(|_| invalid("invalid GPUI keystroke syntax"))?;
            }
        }
        InputCommand::TypeText { text } | InputCommand::ReplaceText { text } => {
            validate_text(text)?;
        }
    }
    Ok(())
}

pub(crate) fn validate_pointer(command: &PointerCommand) -> Result<(), BridgeError> {
    match command {
        PointerCommand::MouseMove { point, .. } => validate_point(*point),
        PointerCommand::MouseDown {
            point, click_count, ..
        }
        | PointerCommand::MouseUp {
            point, click_count, ..
        } => {
            validate_point(*point)?;
            validate_click_count(*click_count)
        }
        PointerCommand::ScrollWheel { point, delta } => {
            validate_point(*point)?;
            let (delta_x, delta_y) = match delta {
                PointerScrollDelta::Pixels { delta_x, delta_y }
                | PointerScrollDelta::Lines { delta_x, delta_y } => (*delta_x, *delta_y),
            };
            validate_scroll_delta(delta_x, delta_y)
        }
    }
}

/// Dispatch one synthetic pointer event through GPUI's native event pipeline.
///
/// Every command becomes the `PlatformInput` variant it already is and lands on
/// [`Window::dispatch_event`], so GPUI's own hit testing, hover, drag, click,
/// and scroll handling run exactly as they do for platform events.
pub(crate) fn dispatch_pointer(
    command: &PointerCommand,
    window: &mut Window,
    cx: &mut App,
) -> Result<(), BridgeError> {
    validate_pointer(command)?;
    let event = match command {
        PointerCommand::MouseMove {
            point: position,
            pressed_button,
        } => PlatformInput::MouseMove(MouseMoveEvent {
            position: native_point(*position),
            pressed_button: pressed_button.map(native_button),
            modifiers: Modifiers::default(),
        }),
        PointerCommand::MouseDown {
            point: position,
            button,
            click_count,
        } => PlatformInput::MouseDown(MouseDownEvent {
            button: native_button(*button),
            position: native_point(*position),
            modifiers: Modifiers::default(),
            click_count: usize::from(*click_count),
            first_mouse: false,
        }),
        PointerCommand::MouseUp {
            point: position,
            button,
            click_count,
        } => PlatformInput::MouseUp(MouseUpEvent {
            button: native_button(*button),
            position: native_point(*position),
            modifiers: Modifiers::default(),
            click_count: usize::from(*click_count),
        }),
        PointerCommand::ScrollWheel {
            point: position,
            delta,
        } => PlatformInput::ScrollWheel(ScrollWheelEvent {
            position: native_point(*position),
            delta: match delta {
                PointerScrollDelta::Pixels { delta_x, delta_y } => {
                    ScrollDelta::Pixels(point(px(*delta_x), px(*delta_y)))
                }
                PointerScrollDelta::Lines { delta_x, delta_y } => {
                    ScrollDelta::Lines(point(*delta_x, *delta_y))
                }
            },
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        }),
    };
    window.dispatch_event(event, cx);
    Ok(())
}

/// Return the GPUI input pipeline's latest pointer position in window-relative logical pixels.
#[must_use]
pub(crate) fn pointer_location(window: &Window) -> Point {
    let position = window.mouse_position();
    Point {
        x: f32::from(position.x),
        y: f32::from(position.y),
    }
}

/// Dispatch one synthetic keyboard command through GPUI's native event pipeline.
///
/// Keystrokes route through [`Window::dispatch_keystroke`], which builds
/// `PlatformInput::KeyDown` and hands it to [`Window::dispatch_event`] before
/// feeding the character to the active input handler, so key bindings and text
/// entry behave exactly as they do for platform keystrokes.
///
/// Text insertion and replacement route through [`Window::insert_input_text`]
/// and [`Window::replace_input_text`], which the C04 patch set opens over the
/// live `PlatformInputHandler` and its `replace_all_text`.
pub(crate) fn dispatch_keyboard(
    command: InputCommand,
    window: &mut Window,
    cx: &mut App,
) -> Result<(), BridgeError> {
    validate(&command)?;
    match command {
        InputCommand::Key { keystroke } => {
            let parsed = Keystroke::parse(&keystroke)
                .map_err(|_| invalid("invalid GPUI keystroke syntax"))?;
            window.dispatch_keystroke(parsed, cx);
        }
        InputCommand::KeySequence { keystrokes } => {
            for keystroke in keystrokes {
                let parsed = Keystroke::parse(&keystroke)
                    .map_err(|_| invalid("invalid GPUI keystroke syntax"))?;
                window.dispatch_keystroke(parsed, cx);
            }
        }
        InputCommand::TypeText { text } => {
            if !window.insert_input_text(&text, cx) {
                return Err(unsupported(
                    "focused element has no active text input handler",
                ));
            }
        }
        InputCommand::ReplaceText { text } => {
            if !window.replace_input_text(&text, cx) {
                return Err(unsupported(
                    "focused input cannot expose its complete document range",
                ));
            }
        }
    }
    Ok(())
}

/// Move keyboard focus to the element tracked by `handle`.
///
/// This is the native replacement for the fork's `Window::focus_observed_element`
/// patch: GPUI's [`Window::focus`] takes a `FocusHandle`, so the caller names the
/// target through its handle and no synthetic key event is involved.
///
/// Mapping a semantic node id to a `FocusHandle` is not possible with the
/// stock 0.3.6 API: `A11y::focus_ids` (`window/a11y.rs:149`) and
/// `FocusHandle::for_id` (`window.rs:558`) are `pub(crate)`. The C04 visibility
/// patch exposes them through [`Window::a11y_focus_handle`], which the bridge's
/// `Focus` operation resolves before calling this function.
pub(crate) fn dispatch_focus(handle: &FocusHandle, window: &mut Window, cx: &mut App) {
    window.focus(handle, cx);
}

fn unsupported(message: &'static str) -> BridgeError {
    BridgeError::new(ErrorCode::Unsupported, message)
}

fn native_point(position: Point) -> gpui::Point<gpui::Pixels> {
    point(px(position.x), px(position.y))
}

const fn native_button(button: MouseButton) -> GpuiMouseButton {
    match button {
        MouseButton::Left => GpuiMouseButton::Left,
        MouseButton::Right => GpuiMouseButton::Right,
        MouseButton::Middle => GpuiMouseButton::Middle,
    }
}

fn validate_click_count(click_count: u8) -> Result<(), BridgeError> {
    if !(1..=3).contains(&click_count) {
        return Err(invalid("click count must be between 1 and 3"));
    }
    Ok(())
}

fn validate_scroll_delta(delta_x: f32, delta_y: f32) -> Result<(), BridgeError> {
    if !delta_x.is_finite() || !delta_y.is_finite() {
        return Err(invalid("scroll deltas must be finite"));
    }
    if delta_x.abs() > 100_000.0 || delta_y.abs() > 100_000.0 {
        return Err(invalid("scroll delta exceeds the safety bound"));
    }
    Ok(())
}

fn validate_point(point: Point) -> Result<(), BridgeError> {
    if !point.is_valid() {
        return Err(invalid("coordinates must be finite"));
    }
    if point.x.abs() > 1_000_000.0 || point.y.abs() > 1_000_000.0 {
        return Err(invalid("coordinates exceed the safety bound"));
    }
    Ok(())
}

fn validate_text(text: &str) -> Result<(), BridgeError> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(invalid("text exceeds the 64 KiB safety bound"));
    }
    Ok(())
}

fn invalid(message: &'static str) -> BridgeError {
    BridgeError::new(ErrorCode::InvalidRequest, message)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{
        AppContext as _, Context, FocusHandle, InteractiveElement as _, IntoElement,
        MouseButton as GpuiMouseButton, ParentElement as _, Render, ScrollDelta,
        StatefulInteractiveElement as _, Styled as _, TestAppContext, VisualTestContext, Window,
        div, point, px, size,
    };
    use gpui_mcp_protocol::{
        InputCommand, MAX_KEY_SEQUENCE, MouseButton, Point, PointerCommand, PointerScrollDelta,
    };

    use super::{dispatch_focus, dispatch_keyboard, dispatch_pointer, validate};

    #[test]
    fn key_sequences_are_bounded_and_fully_validated() {
        assert!(
            validate(&InputCommand::KeySequence {
                keystrokes: vec!["home".to_owned(), "right".to_owned()],
            })
            .is_ok()
        );
        assert!(
            validate(&InputCommand::KeySequence {
                keystrokes: Vec::new(),
            })
            .is_err()
        );
        assert!(
            validate(&InputCommand::KeySequence {
                keystrokes: vec!["right".to_owned(); MAX_KEY_SEQUENCE + 1],
            })
            .is_err()
        );
    }

    #[derive(Clone, Copy)]
    struct DragValue;

    struct DragPreview;

    impl Render for DragPreview {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui::test]
    fn synthetic_mouse_move_runs_gpui_hover_handlers(cx: &mut TestAppContext) {
        let hovered = Rc::new(Cell::new(false));
        let hovered_for_handler = hovered.clone();
        let visual = cx.add_empty_window();
        visual.draw(
            point(px(0.0), px(0.0)),
            size(px(300.0), px(100.0)),
            move |_, _| {
                div()
                    .id("native-hover-target")
                    .w(px(100.0))
                    .h(px(100.0))
                    .on_hover(move |value, _, _| hovered_for_handler.set(*value))
            },
        );

        visual.update(|window, cx| {
            assert_eq!(
                dispatch_pointer(
                    &PointerCommand::MouseMove {
                        point: Point { x: 50.0, y: 50.0 },
                        pressed_button: None,
                    },
                    window,
                    cx,
                ),
                Ok(())
            );
        });

        assert!(hovered.get());
    }

    #[gpui::test]
    fn synthetic_platform_drag_runs_gpui_drag_and_drop_handlers(cx: &mut TestAppContext) {
        let pressed = Rc::new(Cell::new(false));
        let drag_started = Rc::new(Cell::new(false));
        let dropped = Rc::new(Cell::new(false));
        let pressed_for_handler = pressed.clone();
        let drag_started_for_handler = drag_started.clone();
        let dropped_for_handler = dropped.clone();
        let visual = cx.add_empty_window();
        visual.draw(
            point(px(0.0), px(0.0)),
            size(px(300.0), px(100.0)),
            move |_, _| {
                div()
                    .flex()
                    .gap(px(50.0))
                    .child(
                        div()
                            .id("native-drag-source")
                            .w(px(100.0))
                            .h(px(100.0))
                            .on_mouse_down(GpuiMouseButton::Left, move |_, _, _| {
                                pressed_for_handler.set(true);
                            })
                            .on_drag(DragValue, move |_, _, _, cx| {
                                drag_started_for_handler.set(true);
                                cx.new(|_| DragPreview)
                            }),
                    )
                    .child(
                        div()
                            .id("native-drop-target")
                            .w(px(100.0))
                            .h(px(100.0))
                            .on_drop(move |_: &DragValue, _, _| {
                                dropped_for_handler.set(true);
                            }),
                    )
            },
        );

        visual.update(|window, cx| {
            assert_eq!(
                dispatch_pointer(
                    &PointerCommand::MouseDown {
                        point: Point { x: 50.0, y: 50.0 },
                        button: MouseButton::Left,
                        click_count: 1,
                    },
                    window,
                    cx,
                ),
                Ok(())
            );
            assert_eq!(
                dispatch_pointer(
                    &PointerCommand::MouseMove {
                        point: Point { x: 60.0, y: 50.0 },
                        pressed_button: Some(MouseButton::Left),
                    },
                    window,
                    cx,
                ),
                Ok(())
            );
            assert_eq!(
                dispatch_pointer(
                    &PointerCommand::MouseMove {
                        point: Point { x: 200.0, y: 50.0 },
                        pressed_button: Some(MouseButton::Left),
                    },
                    window,
                    cx,
                ),
                Ok(())
            );
            assert_eq!(
                dispatch_pointer(
                    &PointerCommand::MouseUp {
                        point: Point { x: 200.0, y: 50.0 },
                        button: MouseButton::Left,
                        click_count: 1,
                    },
                    window,
                    cx,
                ),
                Ok(())
            );
            assert_eq!(window.mouse_position(), point(px(200.0), px(50.0)));
        });

        assert!(pressed.get());
        assert!(drag_started.get());
        assert!(dropped.get());
    }

    #[gpui::test]
    fn synthetic_scroll_wheel_runs_gpui_scroll_handlers(cx: &mut TestAppContext) {
        let scrolled = Rc::new(Cell::new(0.0_f32));
        let scrolled_for_handler = scrolled.clone();
        let visual = cx.add_empty_window();
        visual.draw(
            point(px(0.0), px(0.0)),
            size(px(300.0), px(100.0)),
            move |_, _| {
                div()
                    .id("native-scroll-target")
                    .w(px(100.0))
                    .h(px(100.0))
                    .on_scroll_wheel(move |event, _, _| {
                        if let ScrollDelta::Pixels(delta) = event.delta {
                            scrolled_for_handler.set(f32::from(delta.y));
                        }
                    })
            },
        );

        visual.update(|window, cx| {
            assert_eq!(
                dispatch_pointer(
                    &PointerCommand::ScrollWheel {
                        point: Point { x: 50.0, y: 50.0 },
                        delta: PointerScrollDelta::Pixels {
                            delta_x: 0.0,
                            delta_y: 120.0,
                        },
                    },
                    window,
                    cx,
                ),
                Ok(())
            );
        });

        assert_eq!(scrolled.get(), 120.0);
    }

    #[gpui::test]
    fn synthetic_keystroke_runs_focused_element_key_handlers(cx: &mut TestAppContext) {
        struct KeyTarget {
            focus: FocusHandle,
            pressed: Rc<Cell<bool>>,
        }

        impl Render for KeyTarget {
            fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
                let pressed = self.pressed.clone();
                div()
                    .id("native-key-target")
                    .track_focus(&self.focus)
                    .w(px(100.0))
                    .h(px(100.0))
                    .on_key_down(move |_, _, _| pressed.set(true))
            }
        }

        let pressed = Rc::new(Cell::new(false));
        let focus = cx.update(|cx| cx.focus_handle());
        let window = cx.add_window({
            let focus = focus.clone();
            let pressed = pressed.clone();
            move |_, _| KeyTarget { focus, pressed }
        });
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        visual.update(|window, cx| {
            window.focus(&focus, cx);
            assert_eq!(
                dispatch_keyboard(
                    InputCommand::Key {
                        keystroke: "enter".to_owned(),
                    },
                    window,
                    cx,
                ),
                Ok(())
            );
        });

        assert!(pressed.get());
    }

    #[gpui::test]
    fn dispatch_focus_moves_focus_without_a_key_event(cx: &mut TestAppContext) {
        let focus = cx.update(|cx| cx.focus_handle());
        let focus_for_draw = focus.clone();
        let visual = cx.add_empty_window();
        visual.draw(
            point(px(0.0), px(0.0)),
            size(px(300.0), px(100.0)),
            move |_, _| {
                div()
                    .id("native-focus-target")
                    .track_focus(&focus_for_draw)
                    .w(px(100.0))
                    .h(px(100.0))
            },
        );

        visual.update(|window, cx| {
            assert!(!focus.is_focused(window));
            dispatch_focus(&focus, window, cx);
            assert!(focus.is_focused(window));
        });
    }

    #[gpui::test]
    fn synthetic_hover_survives_bounds_changed(cx: &mut TestAppContext) {
        let visual = cx.add_empty_window();
        visual.draw(
            point(px(0.0), px(0.0)),
            size(px(300.0), px(100.0)),
            |_, _| div().id("hover-target").w(px(100.0)).h(px(100.0)),
        );

        let synthetic = Point { x: 50.0, y: 50.0 };
        visual.update(|window, cx| {
            assert_eq!(
                dispatch_pointer(
                    &PointerCommand::MouseMove {
                        point: synthetic,
                        pressed_button: None,
                    },
                    window,
                    cx,
                ),
                Ok(())
            );
        });
        assert_eq!(
            visual.update(|window, _| window.mouse_position()),
            point(px(synthetic.x), px(synthetic.y))
        );

        // A resize must not adopt the unchanged platform position and cancel
        // the synthetic hover (C05).
        visual.update(|window, cx| window.bounds_changed(cx));
        assert_eq!(
            visual.update(|window, _| window.mouse_position()),
            point(px(synthetic.x), px(synthetic.y)),
            "the synthetic pointer position survives bounds_changed"
        );

        // The platform's resize callback and its DPI-change path both land in
        // `bounds_changed`, so the hover must survive those paths too.
        visual.simulate_resize(size(px(400.0), px(200.0)));
        assert_eq!(
            visual.update(|window, _| window.mouse_position()),
            point(px(synthetic.x), px(synthetic.y)),
            "the synthetic pointer position survives a resize"
        );
        visual.simulate_scale_factor_change(2.0);
        assert_eq!(
            visual.update(|window, _| window.mouse_position()),
            point(px(synthetic.x), px(synthetic.y)),
            "the synthetic pointer position survives a DPI change"
        );
    }
}
