//! The app, embedded at build time: the server reads nothing from disk except blob
//! bytes (DESIGN §7.4).

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const SVG: &str = "image/svg+xml";

/// The content type and bytes served at `path`, if it is an app file.
pub(crate) fn get(path: &str) -> Option<(&'static str, &'static [u8])> {
    let file: (&'static str, &'static [u8]) = match path {
        "/" => (HTML, include_bytes!("../assets/index.html")),
        "/app.css" => (CSS, include_bytes!("../assets/app.css")),
        "/icon.svg" => (SVG, include_bytes!("../assets/icon.svg")),
        "/js/main.js" => (JS, include_bytes!("../assets/js/main.js")),
        "/js/api.js" => (JS, include_bytes!("../assets/js/api.js")),
        "/js/route.js" => (JS, include_bytes!("../assets/js/route.js")),
        "/js/dom.js" => (JS, include_bytes!("../assets/js/dom.js")),
        "/js/ui.js" => (JS, include_bytes!("../assets/js/ui.js")),
        "/js/claims.js" => (JS, include_bytes!("../assets/js/claims.js")),
        "/js/entities.js" => (JS, include_bytes!("../assets/js/entities.js")),
        "/js/views/search.js" => (JS, include_bytes!("../assets/js/views/search.js")),
        "/js/views/entity.js" => (JS, include_bytes!("../assets/js/views/entity.js")),
        "/js/views/timeline.js" => (JS, include_bytes!("../assets/js/views/timeline.js")),
        "/js/views/gallery.js" => (JS, include_bytes!("../assets/js/views/gallery.js")),
        "/js/views/review.js" => (JS, include_bytes!("../assets/js/views/review.js")),
        _ => return None,
    };
    Some(file)
}
