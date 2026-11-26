//! The output viewer's text area as one custom element: the view lays out
//! the visible rows in `prepaint` (see `OutputView::prepaint_text`), and
//! this paints what it returns: backgrounds (colored text, the prompt band,
//! matches, the selection), the shaped rows, line numbers and scrollbars.

use gpui::{
    App, Bounds, ContentMask, Element, ElementId, Entity, GlobalElementId, Hsla, InspectorElementId, LayoutId, Pixels, Point, ShapedLine,
    Style, Window, fill, relative,
};

use super::OutputView;

pub struct TextArea {
    pub view: Entity<OutputView>,
}

/// Everything one frame paints, in order.
#[derive(Default)]
pub struct Paint {
    pub background: Option<(Bounds<Pixels>, Hsla)>,
    /// Clipped to the text area.
    pub rects: Vec<(Bounds<Pixels>, Hsla)>,
    pub lines: Vec<(Point<Pixels>, ShapedLine)>,
    pub text_clip: Bounds<Pixels>,
    /// Line numbers, clipped to the gutter.
    pub numbers: Vec<(Point<Pixels>, ShapedLine)>,
    pub gutter_clip: Bounds<Pixels>,
    /// Scrollbar tracks and thumbs, unclipped.
    pub bars: Vec<(Bounds<Pixels>, Hsla, Pixels)>,
    pub line_height: Pixels,
}

impl Element for TextArea {
    type RequestLayoutState = ();
    type PrepaintState = Paint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Paint {
        self.view.update(cx, |view, cx| view.prepaint_text(bounds, window, cx))
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut (),
        paint: &mut Paint,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some((b, c)) = paint.background {
            window.paint_quad(fill(b, c));
        }
        let lh = paint.line_height;
        window.with_content_mask(Some(ContentMask { bounds: paint.text_clip }), |window| {
            for (b, c) in paint.rects.drain(..) {
                window.paint_quad(fill(b, c));
            }
            for (origin, line) in &paint.lines {
                let _ = line.paint(*origin, lh, gpui::TextAlign::Left, None, window, cx);
            }
        });
        window.with_content_mask(Some(ContentMask { bounds: paint.gutter_clip }), |window| {
            for (origin, line) in &paint.numbers {
                let _ = line.paint(*origin, lh, gpui::TextAlign::Left, None, window, cx);
            }
        });
        for (b, c, r) in paint.bars.drain(..) {
            window.paint_quad(fill(b, c).corner_radii(r));
        }
    }
}
