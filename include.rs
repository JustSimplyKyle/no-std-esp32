impl AppBuilder for Application {
type PathRouter = impl routing::PathRouter;
fn build_app(self) -> picoserve::Router<Self::PathRouter> {
static_routes!(
    "/home/kyle/coding/controller-ui/target/dx/controller-ui/release/web/public",
    "index.html",
    "assets/tailwind-dxh6e45a68f795d503e.css",
    "assets/controller-ui-dxh8ccd804ba65db57e.js",
    "assets/controller-ui_bg-dxh88c278e3b38c6378.wasm"
)
.route("/controller", post(handle_command))
}}
