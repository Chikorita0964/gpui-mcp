//! Typed computed styles (htmlswap's lowering) onto GPUI styles.
//!
//! htmlswap's [`plan`] decides what each computed value becomes in GPUI,
//! what GPUI draws only approximately and what it cannot draw; the code
//! generator prints the same plan. This module only converts the plan's
//! values into GPUI's, so the renderer, the generated code and diagnostics
//! agree on what is supported.

use std::collections::HashSet;

use gpui::{
    AbsoluteLength, AlignContent, AlignItems, BoxShadow, CursorStyle, DefiniteLength,
    FlexDirection, FlexWrap, FontFallbacks, FontStyle, FontWeight, Length, Overflow, Pixels,
    SharedString, StrikethroughStyle, Styled, TextAlign, UnderlineStyle, Visibility, Window, point,
    px, relative, rems, rgba,
};
use htmlswap::computed::gpui::{
    Absolute, Definite, Features, GpuiAlign, GpuiCursor, GpuiDecoration, GpuiDisplay,
    GpuiDistribute, GpuiLength, GpuiOverflow, GpuiPosition, GpuiStyle, GpuiTextAlign, GpuiTracks,
    Radius, plan,
};
use htmlswap::computed::{
    Bases, ComputedStyle, FlexDirection as CssFlexDirection, FlexWrap as CssFlexWrap, FontFamily,
    FontStyle as CssFontStyle, GridAutoFlow, GridLine, RepeatCount, Rgba, Track, TrackBreadth,
    TrackSize,
};

/// What this renderer's GPUI draws: both backends carry the grid and
/// inset-shadow patches.
const FEATURES: Features = Features { grid_tracks: true, inset_shadows: true };

/// The GPUI color for a computed color.
pub(crate) fn color(color: Rgba) -> gpui::Hsla {
    rgba(color.to_u32()).into()
}

/// The runtime bases lengths resolve against in `window`.
pub(crate) fn bases(window: &Window) -> Bases {
    let viewport = window.viewport_size();
    Bases {
        rem: f32::from(window.rem_size()),
        viewport_width: f32::from(viewport.width),
        viewport_height: f32::from(viewport.height),
    }
}

fn pixels(value: Absolute, bases: &Bases) -> Pixels {
    px(value.length().resolve(bases, 0.0))
}

fn absolute(value: Absolute, bases: &Bases) -> AbsoluteLength {
    match (value.as_px(), value.as_rems()) {
        (Some(pixels), _) => px(pixels).into(),
        // GPUI resolves rems against the window's rem size as it lays out.
        (None, Some(value)) => rems(value).into(),
        (None, None) => pixels(value, bases).into(),
    }
}

fn definite(value: Definite, bases: &Bases) -> DefiniteLength {
    match value {
        Definite::Absolute(value) => absolute(value, bases).into(),
        Definite::Fraction(fraction) => relative(fraction),
    }
}

fn length(value: GpuiLength, bases: &Bases) -> Length {
    match value {
        GpuiLength::Auto => Length::Auto,
        GpuiLength::Definite(value) => Length::Definite(definite(value, bases)),
    }
}

const fn align(value: GpuiAlign) -> AlignItems {
    match value {
        GpuiAlign::Start => AlignItems::Start,
        GpuiAlign::End => AlignItems::End,
        GpuiAlign::FlexStart => AlignItems::FlexStart,
        GpuiAlign::FlexEnd => AlignItems::FlexEnd,
        GpuiAlign::Center => AlignItems::Center,
        GpuiAlign::Baseline => AlignItems::Baseline,
        GpuiAlign::Stretch => AlignItems::Stretch,
    }
}

const fn distribute(value: GpuiDistribute) -> AlignContent {
    match value {
        GpuiDistribute::Start => AlignContent::Start,
        GpuiDistribute::End => AlignContent::End,
        GpuiDistribute::FlexStart => AlignContent::FlexStart,
        GpuiDistribute::FlexEnd => AlignContent::FlexEnd,
        GpuiDistribute::Center => AlignContent::Center,
        GpuiDistribute::Stretch => AlignContent::Stretch,
        GpuiDistribute::SpaceBetween => AlignContent::SpaceBetween,
        GpuiDistribute::SpaceAround => AlignContent::SpaceAround,
        GpuiDistribute::SpaceEvenly => AlignContent::SpaceEvenly,
    }
}

const fn overflow(value: GpuiOverflow) -> Overflow {
    match value {
        GpuiOverflow::Visible => Overflow::Visible,
        GpuiOverflow::Hidden => Overflow::Hidden,
        GpuiOverflow::Clip => Overflow::Clip,
        GpuiOverflow::Scroll => Overflow::Scroll,
    }
}

const fn cursor(value: GpuiCursor) -> CursorStyle {
    match value {
        GpuiCursor::Arrow => CursorStyle::Arrow,
        GpuiCursor::PointingHand => CursorStyle::PointingHand,
        GpuiCursor::IBeam => CursorStyle::IBeam,
        GpuiCursor::IBeamVertical => CursorStyle::IBeamCursorForVerticalLayout,
        GpuiCursor::Crosshair => CursorStyle::Crosshair,
        GpuiCursor::OpenHand => CursorStyle::OpenHand,
        GpuiCursor::ClosedHand => CursorStyle::ClosedHand,
        GpuiCursor::NotAllowed => CursorStyle::OperationNotAllowed,
        GpuiCursor::DragLink => CursorStyle::DragLink,
        GpuiCursor::DragCopy => CursorStyle::DragCopy,
        GpuiCursor::ResizeLeftRight => CursorStyle::ResizeLeftRight,
        GpuiCursor::ResizeUpDown => CursorStyle::ResizeUpDown,
        GpuiCursor::ResizeUpRightDownLeft => CursorStyle::ResizeUpRightDownLeft,
        GpuiCursor::ResizeUpLeftDownRight => CursorStyle::ResizeUpLeftDownRight,
        GpuiCursor::ResizeColumn => CursorStyle::ResizeColumn,
        GpuiCursor::ResizeRow => CursorStyle::ResizeRow,
    }
}

/// The font family GPUI should use: the first installed family in the list,
/// with the rest as fallbacks. Generic families map to the system UI font.
fn font_family(families: &[FontFamily], available: &HashSet<String>) -> (String, Vec<String>) {
    let names = families
        .iter()
        .map(|family| match family {
            FontFamily::Named(name) => name.to_string(),
            _ => ".SystemUIFont".to_owned(),
        })
        .collect::<Vec<_>>();
    let selected = names
        .iter()
        .position(|name| name == ".SystemUIFont" || available.contains(&name.to_ascii_lowercase()));
    match selected {
        Some(index) => (names[index].clone(), names[index + 1..].to_vec()),
        None => (".SystemUIFont".to_owned(), Vec::new()),
    }
}

/// Apply every field the style sets.
pub(crate) fn apply<T: Styled>(
    host: T,
    style: &ComputedStyle,
    fonts: &HashSet<String>,
    bases: &Bases,
) -> T {
    apply_plan(host, &plan(style, FEATURES), fonts, bases)
}

#[allow(clippy::too_many_lines)]
fn apply_plan<T: Styled>(
    mut host: T,
    style: &GpuiStyle,
    fonts: &HashSet<String>,
    bases: &Bases,
) -> T {
    match style.display {
        Some(GpuiDisplay::Hidden) => host = host.hidden(),
        Some(GpuiDisplay::Flex) => host = host.flex(),
        Some(GpuiDisplay::Grid) => host = host.grid(),
        Some(GpuiDisplay::Block) => host = host.block(),
        None => {}
    }
    match style.position {
        Some(GpuiPosition::Absolute) => host = host.absolute(),
        Some(GpuiPosition::Relative) => host = host.relative(),
        None => {}
    }
    {
        let refinement = host.style();
        for (slot, value) in [
            (&mut refinement.inset.top, style.inset.top),
            (&mut refinement.inset.right, style.inset.right),
            (&mut refinement.inset.bottom, style.inset.bottom),
            (&mut refinement.inset.left, style.inset.left),
            (&mut refinement.size.width, style.width),
            (&mut refinement.size.height, style.height),
            (&mut refinement.min_size.width, style.min_width),
            (&mut refinement.min_size.height, style.min_height),
            (&mut refinement.max_size.width, style.max_width),
            (&mut refinement.max_size.height, style.max_height),
            (&mut refinement.margin.top, style.margin.top),
            (&mut refinement.margin.right, style.margin.right),
            (&mut refinement.margin.bottom, style.margin.bottom),
            (&mut refinement.margin.left, style.margin.left),
        ] {
            if let Some(value) = value {
                *slot = Some(length(value, bases));
            }
        }
        if style.aspect_ratio.is_some() {
            refinement.aspect_ratio = style.aspect_ratio;
        }
        for (slot, value) in [
            (&mut refinement.padding.top, style.padding.top),
            (&mut refinement.padding.right, style.padding.right),
            (&mut refinement.padding.bottom, style.padding.bottom),
            (&mut refinement.padding.left, style.padding.left),
            (&mut refinement.gap.width, style.column_gap),
            (&mut refinement.gap.height, style.row_gap),
        ] {
            if let Some(value) = value {
                *slot = Some(definite(value, bases));
            }
        }
        if let Some(direction) = style.flex_direction {
            refinement.flex_direction = Some(match direction {
                CssFlexDirection::Row => FlexDirection::Row,
                CssFlexDirection::RowReverse => FlexDirection::RowReverse,
                CssFlexDirection::Column => FlexDirection::Column,
                CssFlexDirection::ColumnReverse => FlexDirection::ColumnReverse,
            });
        }
        if let Some(wrap) = style.flex_wrap {
            refinement.flex_wrap = Some(match wrap {
                CssFlexWrap::NoWrap => FlexWrap::NoWrap,
                CssFlexWrap::Wrap => FlexWrap::Wrap,
                CssFlexWrap::WrapReverse => FlexWrap::WrapReverse,
            });
        }
        if style.flex_grow.is_some() {
            refinement.flex_grow = style.flex_grow;
        }
        if style.flex_shrink.is_some() {
            refinement.flex_shrink = style.flex_shrink;
        }
        if let Some(basis) = style.flex_basis {
            refinement.flex_basis = Some(length(basis, bases));
        }
        if let Some(value) = style.align_items {
            refinement.align_items = Some(align(value));
        }
        if let Some(value) = style.align_self {
            refinement.align_self = Some(align(value));
        }
        if let Some(value) = style.align_content {
            refinement.align_content = Some(distribute(value));
        }
        if let Some(value) = style.justify_content {
            refinement.justify_content = Some(distribute(value));
        }
        if let Some(value) = style.overflow_x {
            refinement.overflow.x = Some(overflow(value));
        }
        if let Some(value) = style.overflow_y {
            refinement.overflow.y = Some(overflow(value));
        }
        if let Some(visible) = style.visible {
            refinement.visibility =
                Some(if visible { Visibility::Visible } else { Visibility::Hidden });
        }
        if style.opacity.is_some() {
            refinement.opacity = style.opacity;
        }
        if let Some(value) = style.cursor {
            refinement.mouse_cursor = Some(cursor(value));
        }
    }
    host = apply_grid(host, style, bases);
    if let Some(background) = style.background {
        host = host.bg(color(background));
    }
    host = apply_borders(host, style, bases);
    if let Some(shadows) = &style.box_shadow {
        host.style().box_shadow = Some(
            shadows
                .iter()
                .map(|shadow| BoxShadow {
                    color: color(shadow.color),
                    offset: point(pixels(shadow.x, bases), pixels(shadow.y, bases)),
                    blur_radius: pixels(shadow.blur, bases),
                    spread_radius: pixels(shadow.spread, bases),
                    inset: shadow.inset,
                })
                .collect(),
        );
    }
    apply_text(host, style, fonts, bases)
}

fn apply_borders<T: Styled>(mut host: T, style: &GpuiStyle, bases: &Bases) -> T {
    {
        let widths = &mut host.style().border_widths;
        for (slot, value) in [
            (&mut widths.top, style.border_widths.top),
            (&mut widths.right, style.border_widths.right),
            (&mut widths.bottom, style.border_widths.bottom),
            (&mut widths.left, style.border_widths.left),
        ] {
            if let Some(value) = value {
                *slot = Some(absolute(value, bases));
            }
        }
    }
    if style.border_dashed {
        host = host.border_dashed();
    }
    if let Some(border) = style.border_color {
        host = host.border_color(color(border));
    }
    let radii = &mut host.style().corner_radii;
    for (slot, value) in [
        (&mut radii.top_left, style.corner_radii.top_left),
        (&mut radii.top_right, style.corner_radii.top_right),
        (&mut radii.bottom_right, style.corner_radii.bottom_right),
        (&mut radii.bottom_left, style.corner_radii.bottom_left),
    ] {
        match value {
            Some(Radius::Length(value)) => *slot = Some(absolute(value, bases)),
            // GPUI clamps a radius to half the shorter side.
            Some(Radius::Full) => *slot = Some(px(9999.).into()),
            None => {}
        }
    }
    host
}

fn apply_grid<T: Styled>(mut host: T, style: &GpuiStyle, bases: &Bases) -> T {
    match &style.grid_template_columns {
        Some(GpuiTracks::Tracks(tracks)) => {
            if let Some(tracks) = grid_tracks(tracks, bases) {
                host = host.grid_template_columns(tracks);
            }
        }
        Some(GpuiTracks::Count(count)) => host = host.grid_cols(*count),
        None => {}
    }
    match &style.grid_template_rows {
        Some(GpuiTracks::Tracks(tracks)) => {
            if let Some(tracks) = grid_tracks(tracks, bases) {
                host = host.grid_template_rows(tracks);
            }
        }
        Some(GpuiTracks::Count(count)) => host = host.grid_rows(*count),
        None => {}
    }
    if let Some(sizes) =
        style.grid_auto_columns.as_deref().and_then(|sizes| track_sizes(sizes, bases))
    {
        host = host.grid_auto_columns(sizes);
    }
    if let Some(sizes) = style.grid_auto_rows.as_deref().and_then(|sizes| track_sizes(sizes, bases))
    {
        host = host.grid_auto_rows(sizes);
    }
    if let Some(flow) = style.grid_auto_flow {
        host = host.grid_auto_flow(auto_flow(flow));
    }
    let location = host.style().grid_location_mut();
    for (slot, value) in [
        (&mut location.column.start, style.grid_column_start),
        (&mut location.column.end, style.grid_column_end),
        (&mut location.row.start, style.grid_row_start),
        (&mut location.row.end, style.grid_row_end),
    ] {
        if let Some(value) = value {
            *slot = placement(value);
        }
    }
    host
}

const fn auto_flow(flow: GridAutoFlow) -> gpui::GridAutoFlow {
    match (flow.column, flow.dense) {
        (false, false) => gpui::GridAutoFlow::Row,
        (true, false) => gpui::GridAutoFlow::Column,
        (false, true) => gpui::GridAutoFlow::RowDense,
        (true, true) => gpui::GridAutoFlow::ColumnDense,
    }
}

const fn placement(line: GridLine) -> gpui::GridPlacement {
    match line {
        GridLine::Auto => gpui::GridPlacement::Auto,
        GridLine::Line(line) => gpui::GridPlacement::Line(line),
        GridLine::Span(span) => gpui::GridPlacement::Span(span),
    }
}

fn breadth(value: TrackBreadth, bases: &Bases) -> Option<gpui::GridTrackBreadth> {
    Some(match value {
        TrackBreadth::Length(value) => {
            gpui::GridTrackBreadth::Length(definite(Definite::new(value)?, bases))
        }
        TrackBreadth::Flex(fraction) => gpui::GridTrackBreadth::Fraction(fraction),
        TrackBreadth::MinContent => gpui::GridTrackBreadth::MinContent,
        TrackBreadth::MaxContent => gpui::GridTrackBreadth::MaxContent,
        TrackBreadth::Auto => gpui::GridTrackBreadth::Auto,
    })
}

fn track_size(value: TrackSize, bases: &Bases) -> Option<gpui::GridTrackSize> {
    Some(match value {
        TrackSize::Breadth(value) => gpui::GridTrackSize::Breadth(breadth(value, bases)?),
        TrackSize::MinMax(min, max) => {
            gpui::GridTrackSize::MinMax(breadth(min, bases)?, breadth(max, bases)?)
        }
        TrackSize::FitContent(limit) => {
            gpui::GridTrackSize::FitContent(definite(Definite::new(limit)?, bases))
        }
    })
}

fn track_sizes(values: &[TrackSize], bases: &Bases) -> Option<Vec<gpui::GridTrackSize>> {
    values.iter().map(|value| track_size(*value, bases)).collect()
}

fn grid_tracks(tracks: &[Track], bases: &Bases) -> Option<Vec<gpui::GridTrack>> {
    tracks
        .iter()
        .map(|track| {
            Some(match track {
                Track::Size(size) => gpui::GridTrack::Single(track_size(*size, bases)?),
                Track::Repeat { count, tracks } => gpui::GridTrack::Repeat(
                    match count {
                        RepeatCount::Count(count) => gpui::GridRepetition::Count(*count),
                        RepeatCount::AutoFill => gpui::GridRepetition::AutoFill,
                        RepeatCount::AutoFit => gpui::GridRepetition::AutoFit,
                    },
                    track_sizes(tracks, bases)?,
                ),
            })
        })
        .collect()
}

fn apply_text<T: Styled>(
    mut host: T,
    style: &GpuiStyle,
    fonts: &HashSet<String>,
    bases: &Bases,
) -> T {
    if let Some(text) = style.text_color {
        host = host.text_color(color(text));
    }
    if let Some(families) = &style.font_family {
        let (primary, fallbacks) = font_family(families, fonts);
        let text = host.text_style();
        text.font_family = Some(SharedString::from(primary));
        text.font_fallbacks = (!fallbacks.is_empty()).then(|| FontFallbacks::from_fonts(fallbacks));
    }
    if let Some(size) = style.font_size {
        host = host.text_size(absolute(size, bases));
    }
    if let Some(weight) = style.font_weight {
        host = host.font_weight(FontWeight(weight));
    }
    if let Some(font_style) = style.font_style {
        host.text_style().font_style = Some(match font_style {
            CssFontStyle::Normal => FontStyle::Normal,
            CssFontStyle::Italic => FontStyle::Italic,
            CssFontStyle::Oblique => FontStyle::Oblique,
        });
    }
    if let Some(height) = style.line_height {
        host = host.line_height(definite(height, bases));
    }
    if let Some(align) = style.text_align {
        host.text_style().text_align = Some(match align {
            GpuiTextAlign::Left => TextAlign::Left,
            GpuiTextAlign::Center => TextAlign::Center,
            GpuiTextAlign::Right => TextAlign::Right,
        });
    }
    match style.no_wrap {
        Some(true) => host = host.whitespace_nowrap(),
        Some(false) => host = host.whitespace_normal(),
        None => {}
    }
    if style.text_ellipsis {
        host = host.text_ellipsis();
    }
    if let Some(clamp) = style.line_clamp {
        host.text_style().line_clamp = clamp.map(|lines| lines as usize);
    }
    if let Some(decorations) = style.decorations {
        let line = |decoration: GpuiDecoration| (pixels(decoration.thickness, bases), decoration);
        let text = host.text_style();
        text.underline = decorations.underline.map(line).map(|(thickness, line)| UnderlineStyle {
            thickness,
            color: line.color.map(color),
            wavy: line.wavy,
        });
        text.strikethrough = decorations.strikethrough.map(line).map(|(thickness, line)| {
            StrikethroughStyle { thickness, color: line.color.map(color) }
        });
    }
    host
}

/// Values in `style` that GPUI cannot draw, or draws only approximately, as
/// `(property, reason)`.
pub(crate) fn limits(style: &ComputedStyle) -> Vec<(&'static str, &'static str)> {
    let planned = plan(style, FEATURES);
    planned
        .limits
        .iter()
        .chain(&planned.approximations)
        .map(|note| (note.property, note.reason))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use gpui::{Styled, div, px};
    use htmlswap::StyleDeclaration;
    use htmlswap::computed::{Bases, ComputedScope, ComputedStyle, FontFamily, MediaEnvironment};

    use super::{apply, font_family, limits};

    fn computed(declarations: &[(&str, &str)]) -> ComputedStyle {
        let declarations = declarations
            .iter()
            .map(|(property, value)| StyleDeclaration::new(*property, *value, false, None))
            .collect::<Vec<_>>();
        let root = ComputedScope::root(&MediaEnvironment::default());
        let scope = root.child(declarations.iter());
        ComputedStyle::compute(&declarations, &scope.style_context(&root), |_, _| {})
    }

    fn top_border(declarations: &[(&str, &str)]) -> Option<gpui::AbsoluteLength> {
        let mut host = apply(div(), &computed(declarations), &HashSet::new(), &Bases::default());
        host.style().border_widths.top
    }

    #[test]
    fn borders_draw_only_with_a_visible_style() {
        assert_eq!(top_border(&[("border-width", "1px")]), None);
        assert_eq!(
            top_border(&[("border-width", "1px"), ("border-style", "solid")]),
            Some(px(1.).into())
        );
        assert_eq!(
            top_border(&[("border-width", "1px"), ("border-style", "dashed")]),
            Some(px(1.).into())
        );
        assert_eq!(
            top_border(&[("border", "1px solid red"), ("border-style", "hidden")]),
            Some(px(0.).into())
        );
    }

    #[test]
    fn content_box_sizes_include_padding_and_drawn_borders() {
        let width = |declarations: &[(&str, &str)]| {
            let mut host =
                apply(div(), &computed(declarations), &HashSet::new(), &Bases::default());
            host.style().size.width
        };
        let px_width = |pixels: f32| Some(gpui::Length::Definite(px(pixels).into()));

        assert_eq!(
            width(&[("width", "100px"), ("padding", "10px"), ("border", "2px solid")]),
            px_width(124.0)
        );
        assert_eq!(
            width(&[("width", "100px"), ("padding", "10px"), ("border-width", "2px")]),
            px_width(120.0),
            "a border without a style is not drawn and takes no space"
        );
        assert_eq!(
            width(&[("box-sizing", "border-box"), ("width", "100px"), ("padding", "10px")]),
            px_width(100.0)
        );
        assert_eq!(
            width(&[("width", "50%"), ("padding", "10px")]),
            Some(gpui::Length::Definite(gpui::relative(0.5))),
            "GPUI cannot add padding to a relative size: drawn, and diagnosed"
        );
        assert_ne!(
            limits(&computed(&[("width", "50%"), ("padding", "10px")])),
            [] as [(&str, &str); 0]
        );
        assert_eq!(width(&[("width", "50%")]), Some(gpui::Length::Definite(gpui::relative(0.5))));
    }

    #[test]
    fn font_family_picks_the_first_installed_family_and_keeps_case() {
        let available = HashSet::from(["segoe ui".to_owned()]);
        let families = |css: &str| -> Vec<FontFamily> {
            computed(&[("font-family", css)]).font_family.unwrap_or_default()
        };

        assert_eq!(
            font_family(&families("'Segoe UI', sans-serif"), &available),
            ("Segoe UI".to_owned(), vec![".SystemUIFont".to_owned()])
        );
        assert_eq!(
            font_family(&families("system-ui"), &available),
            (".SystemUIFont".to_owned(), Vec::new())
        );
        assert_eq!(
            font_family(&families("'Missing Font', sans-serif"), &available),
            (".SystemUIFont".to_owned(), Vec::new())
        );
    }
}
