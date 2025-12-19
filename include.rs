impl AppBuilder for Application {
type PathRouter = impl routing::PathRouter;
fn build_app(self) -> picoserve::Router<Self::PathRouter> {
static_routes!(
    "/home/kyle/coding/controller-ui/target/dx/controller-ui/release/web/public",
    "index.html",
    "assets/tailwind-dxh445820bbfb4b2713.css",
    "assets/controller-ui-dxhaa1fd7ffa32e1cc8.js",
    "assets/controller-ui_bg-dxh78f198f4602cd8bf.wasm"
)
.route("/controller", post(handle_command))
}}
