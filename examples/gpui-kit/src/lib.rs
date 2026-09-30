//! GPUI Kit controls observed and driven by the ordinary MCP bridge.

use gpui_kit::{
    Context, Entity, IntoElement, Render, Role, Window,
    component::{
        button::Button,
        checkbox::Checkbox,
        input::{Input, InputState},
    },
    div,
    prelude::*,
};
use gpui_mcp::BridgeHandle;

/// A small form that retains its bridge for the window's lifetime.
pub struct Demo {
    count: usize,
    agreed: bool,
    input: Entity<InputState>,
    _bridge: Option<BridgeHandle>,
}

impl Demo {
    /// Create a form; tests attach isolated observation and pass no IPC bridge.
    pub fn new(window: &mut Window, cx: &mut Context<Self>, bridge: Option<BridgeHandle>) -> Self {
        Self {
            count: 0,
            agreed: false,
            input: cx.new(|cx| InputState::new(window, cx).placeholder("Enter your name")),
            _bridge: bridge,
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
            .child(
                Button::new("increment")
                    .label("Increment")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.count += 1;
                        cx.notify();
                    })),
            )
            .child(div().id("count").child(format!("Count: {}", self.count)))
            .child(
                Checkbox::new("agree")
                    .label("Agree")
                    .checked(self.agreed)
                    .on_change(cx.listener(|this, checked, _, cx| {
                        this.agreed = *checked;
                        cx.notify();
                    })),
            )
            .child(Input::new(&self.input).id("name"))
    }
}

#[cfg(test)]
mod tests;
