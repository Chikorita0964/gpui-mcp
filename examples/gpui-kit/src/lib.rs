//! GPUI Kit controls observed and driven by the ordinary MCP bridge.

use gpui_kit::{
    Context, Entity, IntoElement, Render, Role, Window,
    component::{
        button::Button,
        checkbox::Checkbox,
        input::{Input, InputState},
        slider::{Slider, SliderState},
    },
    div,
    prelude::*,
};
use gpui_mcp::{Automation, BridgeHandle};

/// The volume slider's range and its value at launch.
const VOLUME_MAX: f32 = 10.0;
const VOLUME_START: f32 = 5.0;

/// A small form that retains its bridge for the window's lifetime.
pub struct Demo {
    count: usize,
    agreed: bool,
    input: Entity<InputState>,
    /// GPUI Kit's own slider: it registers the Increment and Decrement
    /// accessibility actions and no key handler.
    volume: Entity<SliderState>,
    /// The volume last logged, so only a change writes a line.
    logged_volume: f32,
    /// Where the counter and the volume write their changes: the bridge's log,
    /// which `get_logs` returns. Tests attach their own observation and log
    /// nothing.
    automation: Option<Automation>,
    _bridge: Option<BridgeHandle>,
}

impl Demo {
    /// Create a form; tests attach isolated observation and pass no IPC bridge.
    pub fn new(window: &mut Window, cx: &mut Context<Self>, bridge: Option<BridgeHandle>) -> Self {
        let volume = cx.new(|_| {
            SliderState::new().min(0.0).max(VOLUME_MAX).step(1.0).default_value(VOLUME_START)
        });
        // Kit's slider emits no event when an accessibility action sets its
        // value, so the change is read from the state itself.
        cx.observe(&volume, |this, volume, cx| {
            let value = volume.read(cx).value().end();
            if value != this.logged_volume {
                this.logged_volume = value;
                this.log(&format!("volume changed to {value}"));
            }
        })
        .detach();
        Self {
            count: 0,
            agreed: false,
            input: cx.new(|cx| InputState::new(window, cx).placeholder("Enter your name")),
            volume,
            logged_volume: VOLUME_START,
            automation: bridge.as_ref().map(BridgeHandle::automation),
            _bridge: bridge,
        }
    }

    fn log(&self, message: &str) {
        if let Some(automation) = &self.automation {
            automation.log("info", message);
        }
    }
}

impl Render for Demo {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("kit-demo")
            .role(Role::Application)
            .aria_label("GPUI Kit MCP Demo")
            .size_full()
            .flex()
            .flex_col()
            .p_6()
            .gap_4()
            .child(Button::new("increment").label("Increment").on_click(cx.listener(
                |this, _, _, cx| {
                    this.count += 1;
                    this.log(&format!("counter changed to {}", this.count));
                    cx.notify();
                },
            )))
            .child(div().id("count").child(format!("Count: {}", self.count)))
            .child(Checkbox::new("agree").label("Agree").checked(self.agreed).on_change(
                cx.listener(|this, checked, _, cx| {
                    this.agreed = *checked;
                    cx.notify();
                }),
            ))
            .child(Input::new(&self.input).id("name"))
            .child(div().id("volume-row").w_64().child(Slider::new(&self.volume)))
    }
}

#[cfg(test)]
mod tests;
