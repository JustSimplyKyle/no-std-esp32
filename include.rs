impl AppBuilder for Application {
type PathRouter = impl routing::PathRouter;
fn build_app(self) -> picoserve::Router<Self::PathRouter> {
static_routes!(
    "/home/kyle/coding/controller-ui/target/dx/controller-ui/release/web/public",
    "index.html",
    "assets/tailwind-dxh7da5faa0e3c7893b.css",
    "assets/controller-ui-dxh08610c7c29156b.js",
    "assets/controller-ui_bg-dxh765c15c6c7e41f4f.wasm"
)
.route("/controller", post(handle_command))
}}
