use anyhow::anyhow;
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use image::{GenericImage, GenericImageView, Pixel};
use std::{
    collections::{BTreeSet, HashMap},
    net::SocketAddr,
    path::PathBuf,
    sync::LazyLock,
};
use tera::{Context, Tera};

from_env::config!(
    "Palettizer",
    root: String,
    bind: SocketAddr,
    templates {
        error: PathBuf,
        index: PathBuf,
    }
);

static CONFIG: LazyLock<Config> = LazyLock::new(|| {
    let arg = std::env::args().nth(1).expect("need config filename arg");
    let content = std::fs::read_to_string(arg).expect("could not read config file");
    let mut config = toml::from_str::<Config>(&content).expect("invalid TOML");
    config.hydrate_from_env();
    config
});

static ERROR_TEMPLATE: &str = "error";
static INDEX_TEMPLATE: &str = "index";

static TEMPLATES: LazyLock<Tera> = LazyLock::new(|| {
    let mut tera = Tera::default();

    let error_template =
        std::fs::read_to_string(CONFIG.templates.error.as_path()).expect("no such error template");
    tera.add_raw_template(ERROR_TEMPLATE, &error_template)
        .expect("could not parse error template");

    let index_template =
        std::fs::read_to_string(CONFIG.templates.index.as_path()).expect("no such index template");
    tera.add_raw_template(INDEX_TEMPLATE, &index_template)
        .expect("could not parse index template");

    fn make_url(args: &HashMap<String, tera::Value>) -> tera::Result<tera::Value> {
        let Some(tera::Value::String(text)) = args.get("text") else {
            return Err(tera::Error::from("need string text argument"));
        };
        let Some(tera::Value::String(href)) = args.get("href") else {
            return Err(tera::Error::from("need string href argument"));
        };
        Ok(tera::Value::String(format!(
            "<a href={}>{}</a>",
            href, text
        )))
    }
    tera.register_function("url", make_url);

    tera
});

static PALETTIZE_ENDPOINT: LazyLock<String> =
    LazyLock::new(|| format!("{}/palettize/", CONFIG.root));

static DEFAULT_CONTEXT: LazyLock<Context> = LazyLock::new(|| {
    let mut context = Context::new();
    context.insert("root", &CONFIG.root);
    context.insert("palettize", &*PALETTIZE_ENDPOINT);
    context
});

fn render_error(err: AppError) -> Html<String> {
    let mut context = DEFAULT_CONTEXT.clone();
    context.insert("text", &format!("{}", err.0));
    context.insert("backtrace", &format!("{}", err.0.backtrace()));

    match TEMPLATES.render(ERROR_TEMPLATE, &context) {
        Ok(rendered) => Html::from(rendered),
        Err(err) => Html::from(format!("couldn't render error template {}", err)),
    }
}

struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        tracing::error!("{}: {}", self.0, self.0.backtrace());
        (StatusCode::INTERNAL_SERVER_ERROR, render_error(self)).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(value: anyhow::Error) -> Self {
        AppError(value)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("Bind to {}", CONFIG.bind);

    let app = Router::new()
        .route(&CONFIG.root, get(index))
        .route(&PALETTIZE_ENDPOINT, post(palettize))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(8_000_000));

    let listener = tokio::net::TcpListener::bind(CONFIG.bind).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn not_found() -> Result<Html<String>, AppError> {
    Err(AppError(anyhow!("not found")))
}

async fn index() -> Result<Html<String>, AppError> {
    Ok(render_index().await?)
}

async fn render_index() -> anyhow::Result<Html<String>> {
    Ok(Html::from(
        TEMPLATES.render(INDEX_TEMPLATE, &DEFAULT_CONTEXT)?,
    ))
}

async fn palettize(form: Multipart) -> Result<Response, AppError> {
    Ok(do_palettize(form).await?)
}

async fn do_palettize(mut form: Multipart) -> anyhow::Result<Response> {
    let mut images = HashMap::new();

    while let Some(part) = form.next_field().await? {
        let Some(name) = part.name() else {
            // this sucks
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("need a name"))
                .unwrap());
        };
        let name = name.to_owned();

        use bytes::Buf;
        let bytes = part.bytes().await?;
        let mut data = Vec::with_capacity(bytes.len());
        std::io::copy(&mut bytes.reader(), &mut data)?;

        tracing::debug!("got {}", name);

        images.insert(name, image::load_from_memory(&data));
    }

    let Some(maybe_image) = images.remove("image") else {
        return Ok(Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("need an image"))
            .unwrap());
    };
    let Ok(mut the_image) = maybe_image else {
        return Ok(Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("image is invalid"))
            .unwrap());
    };

    let Some(maybe_palette) = images.remove("palette") else {
        return Ok(Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("need a palette"))
            .unwrap());
    };
    let Ok(the_palette) = maybe_palette else {
        return Ok(Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("palette is invalid"))
            .unwrap());
    };

    let mut colors = BTreeSet::new();
    for (_, _, color) in the_palette.pixels() {
        colors.insert([color.0[0], color.0[1], color.0[2], 255]);
    }
    tracing::debug!("{} colors", colors.len());

    for y in 0..the_image.height() {
        tracing::trace!("row {}", y);
        for x in 0..the_image.width() {
            let color = the_image.get_pixel(x, y);
            let mut min_diff = 255;
            let mut min_color = [0u8, 0, 0, color.0[3]];
            for palette_color in colors.iter() {
                let diff = color.0[0].abs_diff(palette_color[0]) as u64
                    + color.0[1].abs_diff(palette_color[1]) as u64
                    + color.0[2].abs_diff(palette_color[2]) as u64;
                if diff < min_diff {
                    min_diff = diff;
                    min_color = *palette_color;
                }
            }
            min_color[3] = color.0[3];
            the_image.put_pixel(x, y, *image::Rgba::from_slice(&min_color));
        }
    }

    let mut data = std::io::Cursor::new(Vec::new());
    the_image.write_to(&mut data, image::ImageFormat::Png)?;

    Ok(Response::builder()
        .header("Content-Type", "image/png")
        .body(Body::from(data.into_inner()))?
        .into_response())
}
