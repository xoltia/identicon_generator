use std::convert::Infallible;
use std::net::SocketAddr;
use hyper::{Method, Body, Request, Response, Server};
use hyper::service::{make_service_fn, service_fn};
use std::collections::HashMap;
use std::path::Path;
use std::ffi::OsStr;
use std::str::FromStr;
use crypto::digest::Digest;
use crypto::sha2::{Sha224, Sha256, Sha384, Sha512};
use image::{Rgba, GenericImage, DynamicImage, ImageFormat};

type QueryParams<'a> = HashMap<&'a str, &'a str>;

fn parse_query_param_or<'a, T: FromStr + Copy>(query: &QueryParams<'a>, key: &'a str, default: T) -> T {
    query.get(key)
        .map(|s| s.parse::<T>().unwrap_or(default))
        .unwrap_or(default)
}

fn fill_square(img: &mut DynamicImage, x: u32, y: u32, s: u32, c: Rgba<u8>) {
    for py in y..(y+s) {
        for px in x..(x+s) {
            img.put_pixel(px, py, c);
        }
    }
}

fn closest_multiple(n: u32, m: u32) -> u32 {
    (m as f32 * (n as f32 / m as f32).round()) as u32
}

async fn gen_identicon(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    if req.method() != &Method::GET {
        return Ok(
            Response::builder()
                .status(405)
                .body("Only GET requests are supported".into())
                .unwrap()
        );
    }

    let query: QueryParams = req.uri().query()
        .map(|q|
            q.split('&').filter_map(|p| {
                let mut kv = p.split('=');
                kv.next().and_then(|k| kv.next().and_then(|v| Some((k, v))))
            })
            .collect()
        )
        .unwrap_or(HashMap::new());

    let path = Path::new(req.uri().path());
    let grid_size = parse_query_param_or(&query, "size", 5);
    let padding = parse_query_param_or(&query, "pad", 0);
    let resolution = parse_query_param_or(&query, "res", closest_multiple(200, grid_size));
    let symmetrical = parse_query_param_or(&query, "sym", true);
    let file_name = path.file_stem().and_then(OsStr::to_str);
    let extension = path.extension().and_then(OsStr::to_str).unwrap_or("png");
    let cell_size = resolution / grid_size;
    let size = resolution + padding * 2;

    if file_name.is_none() {
        return Ok(Response::builder()
                .status(404)
                .body("No name was provided".into())
                .unwrap()
        );
    }

    if resolution % grid_size != 0 {
        let rounded = closest_multiple(resolution, grid_size);
        return Ok(Response::builder()
            .status(400)
            .header("X-Recommended-Size", rounded)
            .body("The resolution must be evenly divisible by the size".into())
            .unwrap()
        );
    }

    if resolution > 1000 {
        return Ok(Response::builder()
            .status(400)
            .body("Resolution cannot exceed 1000".into())
            .unwrap()
        );
    }

    if size > 256 && extension == "ico" {
        return Ok(Response::builder()
            .status(400)
            .body("ICO size (pad * 2 + res) must be in range 1-256".into())
            .unwrap()
        );
    }

    if cell_size == 0 {
        return Ok(
            Response::builder()
                .status(400)
                .body("Grid size cannot be larger than resolution".into())
                .unwrap()
        );
    }

    // Max of match is floor(sqrt(output_size - 32))
    // because real size of needed minimum output is
    // grid_size * 2 and 32 bits are reserved for color
    let mut hasher: Box<dyn Digest> = match grid_size {
        1..=13 => Box::new(Sha224::new()),
        14 => Box::new(Sha256::new()),
        15..=18 => Box::new(Sha384::new()),
        19..=21 => Box::new(Sha512::new()),
        _ => {
            return Ok(
                Response::builder()
                    .status(400)
                    .body("Grid size must be less from range 1-21".into())
                    .unwrap()
            );
        }
    };

    let mut bytes = Vec::with_capacity(hasher.output_bytes());
    bytes.resize(hasher.output_bytes(), 0);
    hasher.input_str(file_name.unwrap());
    hasher.result(&mut bytes);

    let mut bytes_iter = bytes.into_iter();
    let r = bytes_iter.next().unwrap();
    let g = bytes_iter.next().unwrap();
    let b = bytes_iter.next().unwrap();
    let fill_color = Rgba([r, g, b, 255]);

    let mut bits = bytes_iter
        .map(|byte| {
            vec![
                ((byte >> 0) & 1u8) == 1u8,
                ((byte >> 1) & 1u8) == 1u8,
                ((byte >> 2) & 1u8) == 1u8,
                ((byte >> 3) & 1u8) == 1u8,
                ((byte >> 4) & 1u8) == 1u8,
                ((byte >> 5) & 1u8) == 1u8,
                ((byte >> 6) & 1u8) == 1u8,
                ((byte >> 7) & 1u8) == 1u8,
            ]
        })
        .flatten();

    let mut formatted_buffer = Vec::new();
    let mut img = DynamicImage::new_rgba8(size, size);
    let stop = if symmetrical {
        (resolution as f32 - cell_size as f32 * grid_size as f32 * 0.5f32) as u32
    } else {
        resolution
    };

    for cy in (padding..resolution).step_by(cell_size as usize) {
        for cx in (padding..stop).step_by(cell_size as usize) {
            if bits.next().unwrap() {
                fill_square(&mut img, cx, cy, cell_size, fill_color);
                if symmetrical {
                    fill_square(&mut img, size - cx - cell_size, cy, cell_size, fill_color);
                }
            }
        }
    }

    img.write_to(&mut formatted_buffer, match extension {
        "bmp" => ImageFormat::Bmp,
        "jpeg" => ImageFormat::Jpeg,
        "ico" => ImageFormat::Ico,
        _ => ImageFormat::Png,
    }).expect("Unable to write to formatted buffer");

    Ok(
        Response::builder()
            .header("Content-Type", format!("image/{}", extension))
            .body(Body::from(formatted_buffer))
            .unwrap()
    )
}

#[tokio::main]
async fn main() {
    let port = std::env::var("PORT")
        .expect("Expected PORT environment variable")
        .parse::<u16>()
        .expect("Could not parse PORT environment variable");

    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let make_svc = make_service_fn(|_conn| async {
        Ok::<_, Infallible>(service_fn(gen_identicon))
    });

    let server = Server::bind(&addr).serve(make_svc);

    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
}
