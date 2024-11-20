use std::sync::Arc;
use std::path::PathBuf;
use std::fs;
use resvg::{self, tiny_skia, usvg};
use usvg::{Tree, Options, fontdb};
use resvg::tiny_skia::{Pixmap, Color};
use anyhow::{Result, Context};
use lopdf::{Document, Object, Stream, Dictionary, content::{Content, Operation}};
use clap::Parser;
use rayon::prelude::*;
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::Mutex;

#[derive(Parser)]
#[command(author, version, about = "Convert directory of SVGs to PDF")]
struct Cli {
    /// Input directory containing SVG files
    #[arg(short, long)]
    input_dir: PathBuf,

    /// Output PDF file
    #[arg(short, long)]
    output: PathBuf,

    /// Scale factor (e.g., 1.0 for original size)
    #[arg(short, long, default_value = "0.1")]
    scale: f32,
}

// Structure to hold rendered page data
struct PageData {
    index: usize,
    width: u32,
    height: u32,
    rgb_data: Vec<u8>,
}

fn main() -> Result<()> {
    let args = Cli::parse();

    // Set up font database
    let mut fontdb = fontdb::Database::new();
    fontdb.load_system_fonts();

    // Create options and set font database
    let mut opt = Options::default();
    opt.fontdb = Arc::from(fontdb);
    let opt = Arc::new(opt);

    // Get all SVG files from directory
    let entries: Vec<_> = fs::read_dir(&args.input_dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.path()
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_lowercase() == "svg")
                .unwrap_or(false)
        })
        .collect();

    if entries.is_empty() {
        anyhow::bail!("No SVG files found in directory");
    }

    // Set up progress bar
    let progress_bar = Arc::new(ProgressBar::new(entries.len() as u64));
    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} {msg}")
            .unwrap()
    );

    // Process SVGs in parallel
    let scale = args.scale;
    let rendered_pages: Vec<PageData> = entries.par_iter().enumerate()
        .map(|(index, entry)| {
            let opt = Arc::clone(&opt);
            let progress_bar = Arc::clone(&progress_bar);

            // Read and parse SVG
            let svg_data = fs::read(entry.path())
                .with_context(|| format!("Failed to read SVG file: {:?}", entry.path()))?;

            // Parse SVG tree
            let tree = Tree::from_data(&svg_data, &opt)
                .with_context(|| format!("Failed to parse SVG file: {:?}", entry.path()))?;

            // Get size and apply scaling
            let size = tree.size();
            let width = (size.width() * scale) as u32;
            let height = (size.height() * scale) as u32;

            // Create pixel buffer with white background
            let mut pixmap = Pixmap::new(width, height)
                .context("Failed to create pixel buffer")?;

            // Fill with white background
            let mut pixmap_mut = pixmap.as_mut();
            pixmap_mut.fill(Color::from_rgba8(255, 255, 255, 255));

            // Create transform with scaling
            let transform = tiny_skia::Transform::from_scale(scale, scale);

            // Render SVG over the white background
            resvg::render(&tree, transform, &mut pixmap_mut);

            // Convert pixmap to RGB data
            let rgb_data: Vec<u8> = pixmap.data()
                .chunks(4)
                .flat_map(|chunk| chunk[0..3].to_vec())
                .collect();

            progress_bar.inc(1);
            progress_bar.set_message(format!("Processed {:?}", entry.path().file_name().unwrap_or_default()));

            Ok(PageData {
                index,
                width,
                height,
                rgb_data,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    progress_bar.finish_with_message("Rendering complete. Creating PDF...");

    // Create PDF document
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let mut page_ids = Vec::new();

    // Create pages in original order
    for page in rendered_pages.iter() {
        // Create image dictionary
        let image_dict = Dictionary::from_iter(vec![
            ("Type", Object::Name("XObject".as_bytes().to_vec())),
            ("Subtype", Object::Name("Image".as_bytes().to_vec())),
            ("Width", Object::Integer(page.width as i64)),
            ("Height", Object::Integer(page.height as i64)),
            ("ColorSpace", Object::Name("DeviceRGB".as_bytes().to_vec())),
            ("BitsPerComponent", Object::Integer(8)),
        ]);

        // Create image stream
        let image_stream = Stream::new(image_dict, page.rgb_data.clone());
        let image_ref = doc.add_object(Object::Stream(image_stream));

        // Create content operations
        let content_operations = vec![
            Operation::new("q", vec![]),
            Operation::new("cm", vec![
                Object::Real(page.width as f32),
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(page.height as f32),
                Object::Real(0.0),
                Object::Real(0.0),
            ]),
            Operation::new("Do", vec![Object::Name("Im1".as_bytes().to_vec())]),
            Operation::new("Q", vec![]),
        ];

        // Create content stream
        let content = Content { operations: content_operations };
        let content_stream = Stream::new(Dictionary::new(), content.encode().unwrap());
        let content_id = doc.add_object(Object::Stream(content_stream));

        // Create resources dictionary
        let xobjects = Dictionary::from_iter(vec![
            ("Im1", Object::Reference(image_ref))
        ]);

        let resources = Dictionary::from_iter(vec![
            ("XObject", Object::Dictionary(xobjects))
        ]);
        let resources_id = doc.add_object(Object::Dictionary(resources));

        // Create page object
        let page_dict = Dictionary::from_iter(vec![
            ("Type", Object::Name("Page".as_bytes().to_vec())),
            ("Parent", Object::Reference(pages_id)),
            ("MediaBox", Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(page.width as i64),
                Object::Integer(page.height as i64),
            ])),
            ("Resources", Object::Reference(resources_id)),
            ("Contents", Object::Reference(content_id)),
        ]);
        let page_id = doc.add_object(Object::Dictionary(page_dict));
        page_ids.push(Object::Reference(page_id));
    }

    // Create pages object
    let pages_dict = Dictionary::from_iter(vec![
        ("Type", Object::Name("Pages".as_bytes().to_vec())),
        ("Count", Object::Integer(page_ids.len() as i64)),
        ("Kids", Object::Array(page_ids)),
    ]);
    doc.objects.insert(pages_id, Object::Dictionary(pages_dict));

    // Create catalog
    let catalog_dict = Dictionary::from_iter(vec![
        ("Type", Object::Name("Catalog".as_bytes().to_vec())),
        ("Pages", Object::Reference(pages_id)),
    ]);
    let catalog_id = doc.add_object(Object::Dictionary(catalog_dict));
    doc.trailer.set("Root", Object::Reference(catalog_id));

    // Save PDF
    doc.save(&args.output)?;

    println!("PDF created successfully with {} pages!", entries.len());
    Ok(())
}