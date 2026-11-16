//! Realized images compare by their data, not by the file that loaded them.
//!
//! `ImageElem::source` is a `Derived<DataSource, Loaded>`, and
//! `Loaded::source` is a `Spanned<LoadSource>`. Its span points into the
//! document that called `image(...)`. The old and new documents are separate
//! source files, so the same picture loaded from the same file never compares
//! equal across them, and every unchanged image became an opaque replacement.
//!
//! Both evaluated trees are normalized after evaluation, so images nested in
//! figures, boxes, tables and other containers compare by data. Images that
//! show rules create during realization pass through the diff library's
//! wrapper around Typst's built-in image show rule, which detaches the load
//! span and then delegates to the built-in rule unchanged. The span only
//! locates diagnostics; layout uses the loaded data and the image fields.

use std::sync::OnceLock;

use typst::diag::SourceResult;
use typst::engine::Engine;
use typst::foundations::StyledElem;
use typst::foundations::{
    Bytes, Content, Derived, NativeElement, NativeRuleMap, NativeShowRule, Packed, ShowFn,
    StyleChain, Target,
};
use typst::loading::{DataSource, LoadSource, Loaded};
use typst::syntax::Spanned;
use typst::visualize::ImageElem;

use crate::container_ops::{realized_child_contents, replace_realized_child};

static BUILTIN_IMAGE_RULE: OnceLock<NativeShowRule> = OnceLock::new();

/// Replace the paged image rule in `rules` by a wrapper around the built-in
/// rule that realizes images without their load span.
pub(crate) fn install_span_free_image_rule(rules: &mut NativeRuleMap) {
    let empty = || Bytes::new(Vec::<u8>::new());
    let probe = ImageElem::new(Derived::new(
        DataSource::Bytes(empty()),
        Loaded::new(Spanned::detached(LoadSource::Bytes), empty()),
    ))
    .pack();
    let Some(builtin) = rules.get(Target::Paged, &probe) else {
        return;
    };
    BUILTIN_IMAGE_RULE.get_or_init(|| builtin);
    rules.replace(Target::Paged, span_free_image_rule as ShowFn<ImageElem>);
}

fn span_free_image_rule(
    elem: &Packed<ImageElem>,
    engine: &mut Engine,
    styles: StyleChain,
) -> SourceResult<Content> {
    let builtin = BUILTIN_IMAGE_RULE
        .get()
        .expect("installed together with the built-in image rule");
    builtin.apply(&without_load_span(elem).pack(), engine, styles)
}

fn without_load_span(elem: &Packed<ImageElem>) -> Packed<ImageElem> {
    let mut elem = elem.clone();
    let loaded = &elem.source.derived;
    let detached = Loaded::new(
        Spanned::detached(loaded.source.v.clone()),
        loaded.data.clone(),
    );
    elem.source = Derived::new(elem.source.source.clone(), detached);
    elem
}

/// Detach the load span of every image reachable through the structured
/// children of `content`.
pub(crate) fn detach_image_load_spans(content: Content) -> Content {
    detached(&content).unwrap_or(content)
}

/// The rewritten content, or `None` if no image below `content` carried a
/// load span.
fn detached(content: &Content) -> Option<Content> {
    if let Some(image) = content.to_packed::<ImageElem>() {
        if image.source.derived.source.span.is_detached() {
            return None;
        }
        return Some(without_load_span(image).pack());
    }
    // Recurse into the stored child: `realized_child_contents` materializes
    // style-dependent fields of styled children, which must not leak into the
    // normalized tree.
    if let Some(styled) = content.to_packed::<StyledElem>() {
        let child = detached(&styled.child)?;
        let mut result = content.clone();
        result.to_packed_mut::<StyledElem>()?.child = child;
        return Some(result);
    }
    let mut result: Option<Content> = None;
    for (index, child) in realized_child_contents(content).iter().enumerate() {
        if let Some(child) = detached(child)
            && let Some(replaced) =
                replace_realized_child(result.as_ref().unwrap_or(content), index, child)
        {
            result = Some(replaced);
        }
    }
    result
}
