use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Timelike, Utc};
use codespan_reporting::diagnostic::{Diagnostic, Label};
use codespan_reporting::term;
use ecow::{eco_format, EcoString};
use parking_lot::RwLock;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use typst::diag::{
    bail, At, Severity, SourceDiagnostic, SourceResult, StrResult, Warned,
};
use typst::foundations::{Datetime, Smart};
use typst::html::HtmlDocument;
use typst::layout::{Frame, Page, PageRanges, PagedDocument};
use typst::syntax::{FileId, Source, Span};
use typst::WorldExt;
use typst_pdf::{PdfOptions, PdfStandards};

use crate::args::{
    CompileArgs, CompileCommand, DiagnosticFormat, Input, Output, OutputFormat,
    PdfStandard,
};
use crate::timings::Timer;

use crate::watch::Status;
use crate::world::SystemWorld;
use crate::{set_failed, terminal};

type CodespanResult<T> = Result<T, CodespanError>;
type CodespanError = codespan_reporting::files::Error;

/// Execute a compilation command.
pub fn compile(timer: &mut Timer, command: &CompileCommand) -> StrResult<()> {
    let mut config = CompileConfig::new(&command.args)?;
    let mut world =
        SystemWorld::new(&command.args.input, &command.args.world, &command.args.process)
            .map_err(|err| eco_format!("{err}"))?;
    timer.record(&mut world, |world| compile_once(world, &mut config, false))?
}

/// A preprocessed `CompileCommand`.
pub struct CompileConfig {
    /// Path to input Typst file or stdin.
    pub input: Input,
    /// Path to output file (PDF, PNG, SVG, or HTML).
    pub output: Output,
    /// The format of the output file.
    pub output_format: OutputFormat,
    /// Which pages to export.
    pub pages: Option<PageRanges>,
    /// The document's creation date formatted as a UNIX timestamp.
    pub creation_timestamp: Option<DateTime<Utc>>,
    /// The format to emit diagnostics in.
    pub diagnostic_format: DiagnosticFormat,
    /// Opens the output file with the default viewer or a specific program after
    /// compilation.
    pub open: Option<Option<String>>,
    /// One (or multiple comma-separated) PDF standards that Typst will enforce
    /// conformance with.
    pub pdf_standards: PdfStandards,
    /// A path to write a Makefile rule describing the current compilation.
    pub make_deps: Option<PathBuf>,
    /// The PPI (pixels per inch) to use for PNG export.
    pub ppi: f32,
}

impl CompileConfig {
    /// Preprocess a `CompileCommand`, producing a compilation config.
    pub fn new(args: &CompileArgs) -> StrResult<Self> {
        let input = args.input.clone();

        let output_format = if let Some(specified) = args.format {
            specified
        } else if let Some(Output::Path(output)) = &args.output {
            match output.extension() {
                Some(ext) if ext.eq_ignore_ascii_case("pdf") => OutputFormat::Pdf,
                Some(ext) if ext.eq_ignore_ascii_case("png") => OutputFormat::Png,
                Some(ext) if ext.eq_ignore_ascii_case("svg") => OutputFormat::Svg,
                Some(ext) if ext.eq_ignore_ascii_case("html") => OutputFormat::Html,
                _ => bail!(
                    "could not infer output format for path {}.\n\
                     consider providing the format manually with `--format/-f`",
                    output.display()
                ),
            }
        } else {
            OutputFormat::Pdf
        };

        let output = args.output.clone().unwrap_or_else(|| {
            let Input::Path(path) = &input else {
                panic!("output must be specified when input is from stdin, as guarded by the CLI");
            };
            Output::Path(path.with_extension(
                match output_format {
                    OutputFormat::Pdf => "pdf",
                    OutputFormat::Png => "png",
                    OutputFormat::Svg => "svg",
                    OutputFormat::Html => "html",
                },
            ))
        });

        let pages = args.pages.as_ref().map(|export_ranges| {
            PageRanges::new(export_ranges.iter().map(|r| r.0.clone()).collect())
        });

        let pdf_standards = {
            let list = args
                .pdf_standard
                .iter()
                .map(|standard| match standard {
                    PdfStandard::V_1_7 => typst_pdf::PdfStandard::V_1_7,
                    PdfStandard::A_2b => typst_pdf::PdfStandard::A_2b,
                })
                .collect::<Vec<_>>();
            PdfStandards::new(&list)?
        };

        Ok(Self {
            input,
            output,
            output_format,
            pages,
            pdf_standards,
            creation_timestamp: args.world.creation_timestamp,
            make_deps: args.make_deps.clone(),
            ppi: args.ppi,
            diagnostic_format: args.process.diagnostic_format,
            open: args.open.clone(),
        })
    }
}

/// Compile a single time.
///
/// Returns whether it compiled without errors.
#[typst_macros::time(name = "compile once")]
pub fn compile_once(
    world: &mut SystemWorld,
    config: &mut CompileConfig,
    watching: bool,
) -> StrResult<()> {
    let start = std::time::Instant::now();
    if watching {
        Status::Compiling.print(config).unwrap();
    }

    let Warned { output, warnings } = compile_and_export(world, config, watching);

    match output {
        // Export the PDF / PNG.
        Ok(()) => {
            let duration = start.elapsed();

            if watching {
                if warnings.is_empty() {
                    Status::Success(duration).print(config).unwrap();
                } else {
                    Status::PartialSuccess(duration).print(config).unwrap();
                }
            }

            print_diagnostics(world, &[], &warnings, config.diagnostic_format)
                .map_err(|err| eco_format!("failed to print diagnostics ({err})"))?;

            write_make_deps(world, config)?;

            if let Some(open) = config.open.take() {
                if let Output::Path(file) = &config.output {
                    open_file(open.as_deref(), file)?;
                }
            }
        }

        // Print diagnostics.
        Err(errors) => {
            set_failed();

            if watching {
                Status::Error.print(config).unwrap();
            }

            print_diagnostics(world, &errors, &warnings, config.diagnostic_format)
                .map_err(|err| eco_format!("failed to print diagnostics ({err})"))?;
        }
    }

    Ok(())
}

fn compile_and_export(
    world: &mut SystemWorld,
    config: &mut CompileConfig,
    watching: bool,
) -> Warned<SourceResult<()>> {
    match config.output_format {
        OutputFormat::Html => {
            let Warned { output, warnings } = typst::compile::<HtmlDocument>(world);
            let result = output.and_then(|document| {
                config
                    .output
                    .write(typst_html::html(&document)?.as_bytes())
                    .map_err(|err| eco_format!("failed to write HTML file ({err})"))
                    .at(Span::detached())
            });
            Warned { output: result, warnings }
        }
        _ => {
            let Warned { output, warnings } = typst::compile::<PagedDocument>(world);
            let result = output
                .and_then(|document| export_paged(world, &document, config, watching));
            Warned { output: result, warnings }
        }
    }
}

/// Export into the target format.
fn export_paged(
    world: &mut SystemWorld,
    document: &PagedDocument,
    config: &CompileConfig,
    watching: bool,
) -> SourceResult<()> {
    match config.output_format {
        OutputFormat::Pdf => export_pdf(document, config),
        OutputFormat::Png => {
            export_image(world, document, config, watching, ImageExportFormat::Png)
                .at(Span::detached())
        }
        OutputFormat::Svg => {
            export_image(world, document, config, watching, ImageExportFormat::Svg)
                .at(Span::detached())
        }
        OutputFormat::Html => unreachable!(),
    }
}

/// Export to a PDF.
fn export_pdf(document: &PagedDocument, config: &CompileConfig) -> SourceResult<()> {
    let options = PdfOptions {
        ident: Smart::Auto,
        timestamp: convert_datetime(
            config.creation_timestamp.unwrap_or_else(chrono::Utc::now),
        ),
        page_ranges: config.pages.clone(),
        standards: config.pdf_standards.clone(),
    };
    let buffer = typst_pdf::pdf(document, &options)?;
    config
        .output
        .write(&buffer)
        .map_err(|err| eco_format!("failed to write PDF file ({err})"))
        .at(Span::detached())?;
    Ok(())
}

/// Convert [`chrono::DateTime`] to [`Datetime`]
fn convert_datetime(date_time: chrono::DateTime<chrono::Utc>) -> Option<Datetime> {
    Datetime::from_ymd_hms(
        date_time.year(),
        date_time.month().try_into().ok()?,
        date_time.day().try_into().ok()?,
        date_time.hour().try_into().ok()?,
        date_time.minute().try_into().ok()?,
        date_time.second().try_into().ok()?,
    )
}

/// An image format to export in.
#[derive(Clone, Copy)]
enum ImageExportFormat {
    Png,
    Svg,
}

/// Export to one or multiple images.
fn export_image(
    world: &mut SystemWorld,
    document: &PagedDocument,
    config: &CompileConfig,
    watching: bool,
    fmt: ImageExportFormat,
) -> StrResult<()> {
    // Determine whether we have indexable templates in output
    let can_handle_multiple = match config.output {
        Output::Stdout => false,
        Output::Path(ref output) => {
            output_template::has_indexable_template(output.to_str().unwrap_or_default())
        }
    };

    let exported_pages = document
        .pages
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            config.pages.as_ref().map_or(true, |exported_page_ranges| {
                exported_page_ranges.includes_page_index(*i)
            })
        })
        .collect::<Vec<_>>();

    if !can_handle_multiple && exported_pages.len() > 1 {
        let err = match config.output {
            Output::Stdout => "to stdout",
            Output::Path(_) => {
                "without a page number template ({p}, {0p}) in the output path"
            }
        };
        bail!("cannot export multiple images {err}");
    }

    let cache = world.export_cache();

    // The results are collected in a `Vec<()>` which does not allocate.
    exported_pages
        .par_iter()
        .map(|(i, page)| {
            // Use output with converted path.
            let output = match &config.output {
                Output::Path(path) => {
                    let storage;
                    let path = if can_handle_multiple {
                        storage = output_template::format(
                            path.to_str().unwrap_or_default(),
                            i + 1,
                            document.pages.len(),
                        );
                        Path::new(&storage)
                    } else {
                        path
                    };

                    // If we are not watching, don't use the cache.
                    // If the frame is in the cache, skip it.
                    // If the file does not exist, always create it.
                    if watching && cache.is_cached(*i, &page.frame) && path.exists() {
                        return Ok(());
                    }

                    Output::Path(path.to_owned())
                }
                Output::Stdout => Output::Stdout,
            };

            export_image_page(config, page, &output, fmt)?;
            Ok(())
        })
        .collect::<Result<Vec<()>, EcoString>>()?;

    Ok(())
}

mod output_template {
    const INDEXABLE: [&str; 3] = ["{p}", "{0p}", "{n}"];

    pub fn has_indexable_template(output: &str) -> bool {
        INDEXABLE.iter().any(|template| output.contains(template))
    }

    pub fn format(output: &str, this_page: usize, total_pages: usize) -> String {
        // Find the base 10 width of number `i`
        fn width(i: usize) -> usize {
            1 + i.checked_ilog10().unwrap_or(0) as usize
        }

        let other_templates = ["{t}"];
        INDEXABLE.iter().chain(other_templates.iter()).fold(
            output.to_string(),
            |out, template| {
                let replacement = match *template {
                    "{p}" => format!("{this_page}"),
                    "{0p}" | "{n}" => format!("{:01$}", this_page, width(total_pages)),
                    "{t}" => format!("{total_pages}"),
                    _ => unreachable!("unhandled template placeholder {template}"),
                };
                out.replace(template, replacement.as_str())
            },
        )
    }
}

/// Export single image.
fn export_image_page(
    config: &CompileConfig,
    page: &Page,
    output: &Output,
    fmt: ImageExportFormat,
) -> StrResult<()> {
    match fmt {
        ImageExportFormat::Png => {
            let pixmap = typst_render::render(page, config.ppi / 72.0);
            let buf = pixmap
                .encode_png()
                .map_err(|err| eco_format!("failed to encode PNG file ({err})"))?;
            output
                .write(&buf)
                .map_err(|err| eco_format!("failed to write PNG file ({err})"))?;
        }
        ImageExportFormat::Svg => {
            let svg = typst_svg::svg(page);
            output
                .write(svg.as_bytes())
                .map_err(|err| eco_format!("failed to write SVG file ({err})"))?;
        }
    }
    Ok(())
}

impl Output {
    fn write(&self, buffer: &[u8]) -> StrResult<()> {
        match self {
            Output::Stdout => std::io::stdout().write_all(buffer),
            Output::Path(path) => fs::write(path, buffer),
        }
        .map_err(|err| eco_format!("{err}"))
    }
}

/// Caches exported files so that we can avoid re-exporting them if they haven't
/// changed.
///
/// This is done by having a list of size `files.len()` that contains the hashes
/// of the last rendered frame in each file. If a new frame is inserted, this
/// will invalidate the rest of the cache, this is deliberate as to decrease the
/// complexity and memory usage of such a cache.
pub struct ExportCache {
    /// The hashes of last compilation's frames.
    pub cache: RwLock<Vec<u128>>,
}

impl ExportCache {
    /// Creates a new export cache.
    pub fn new() -> Self {
        Self { cache: RwLock::new(Vec::with_capacity(32)) }
    }

    /// Returns true if the entry is cached and appends the new hash to the
    /// cache (for the next compilation).
    pub fn is_cached(&self, i: usize, frame: &Frame) -> bool {
        let hash = typst::utils::hash128(frame);

        let mut cache = self.cache.upgradable_read();
        if i >= cache.len() {
            cache.with_upgraded(|cache| cache.push(hash));
            return false;
        }

        cache.with_upgraded(|cache| std::mem::replace(&mut cache[i], hash) == hash)
    }
}

/// Writes a Makefile rule describing the relationship between the output and
/// its dependencies to the path specified by the --make-deps argument, if it
/// was provided.
fn write_make_deps(world: &mut SystemWorld, config: &CompileConfig) -> StrResult<()> {
    let Some(ref make_deps_path) = config.make_deps else { return Ok(()) };
    let Output::Path(output_path) = &config.output else {
        bail!("failed to create make dependencies file because output was stdout")
    };
    let Some(output_path) = output_path.as_os_str().to_str() else {
        bail!("failed to create make dependencies file because output path was not valid unicode")
    };

    // Based on `munge` in libcpp/mkdeps.cc from the GCC source code. This isn't
    // perfect as some special characters can't be escaped.
    fn munge(s: &str) -> String {
        let mut res = String::with_capacity(s.len());
        let mut slashes = 0;
        for c in s.chars() {
            match c {
                '\\' => slashes += 1,
                '$' => {
                    res.push('$');
                    slashes = 0;
                }
                ' ' | '\t' => {
                    // `munge`'s source contains a comment here that says: "A
                    // space or tab preceded by 2N+1 backslashes represents N
                    // backslashes followed by space..."
                    for _ in 0..slashes + 1 {
                        res.push('\\');
                    }
                    slashes = 0;
                }
                '#' => {
                    res.push('\\');
                    slashes = 0;
                }
                _ => slashes = 0,
            };
            res.push(c);
        }
        res
    }

    fn write(
        make_deps_path: &Path,
        output_path: &str,
        root: PathBuf,
        dependencies: impl Iterator<Item = PathBuf>,
    ) -> io::Result<()> {
        let mut file = File::create(make_deps_path)?;

        file.write_all(munge(output_path).as_bytes())?;
        file.write_all(b":")?;
        for dependency in dependencies {
            let Some(dependency) =
                dependency.strip_prefix(&root).unwrap_or(&dependency).to_str()
            else {
                // Silently skip paths that aren't valid unicode so we still
                // produce a rule that will work for the other paths that can be
                // processed.
                continue;
            };

            file.write_all(b" ")?;
            file.write_all(munge(dependency).as_bytes())?;
        }
        file.write_all(b"\n")?;

        Ok(())
    }

    write(make_deps_path, output_path, world.root().to_owned(), world.dependencies())
        .map_err(|err| {
            eco_format!("failed to create make dependencies file due to IO error ({err})")
        })
}

/// Opens the given file using:
/// - The default file viewer if `open` is `None`.
/// - The given viewer provided by `open` if it is `Some`.
///
/// If the file could not be opened, an error is returned.
fn open_file(open: Option<&str>, path: &Path) -> StrResult<()> {
    // Some resource openers require the path to be canonicalized.
    let path = path
        .canonicalize()
        .map_err(|err| eco_format!("failed to canonicalize path ({err})"))?;
    if let Some(app) = open {
        open::with_detached(&path, app)
            .map_err(|err| eco_format!("failed to open file with {} ({})", app, err))
    } else {
        open::that_detached(&path).map_err(|err| {
            let openers = open::commands(path)
                .iter()
                .map(|command| command.get_program().to_string_lossy())
                .collect::<Vec<_>>()
                .join(", ");
            eco_format!(
                "failed to open file with any of these resource openers: {} ({})",
                openers,
                err,
            )
        })
    }
}

/// Print diagnostic messages to the terminal.
pub fn print_diagnostics(
    world: &SystemWorld,
    errors: &[SourceDiagnostic],
    warnings: &[SourceDiagnostic],
    diagnostic_format: DiagnosticFormat,
) -> Result<(), codespan_reporting::files::Error> {
    let mut config = term::Config { tab_width: 2, ..Default::default() };
    if diagnostic_format == DiagnosticFormat::Short {
        config.display_style = term::DisplayStyle::Short;
    }

    for diagnostic in warnings.iter().chain(errors) {
        let diag = match diagnostic.severity {
            Severity::Error => Diagnostic::error(),
            Severity::Warning => Diagnostic::warning(),
        }
        .with_message(diagnostic.message.clone())
        .with_notes(
            diagnostic
                .hints
                .iter()
                .map(|e| (eco_format!("hint: {e}")).into())
                .collect(),
        )
        .with_labels(label(world, diagnostic.span).into_iter().collect());

        term::emit(&mut terminal::out(), &config, world, &diag)?;

        // Stacktrace-like helper diagnostics.
        for point in &diagnostic.trace {
            let message = point.v.to_string();
            let help = Diagnostic::help()
                .with_message(message)
                .with_labels(label(world, point.span).into_iter().collect());

            term::emit(&mut terminal::out(), &config, world, &help)?;
        }
    }

    Ok(())
}

/// Create a label for a span.
fn label(world: &SystemWorld, span: Span) -> Option<Label<FileId>> {
    Some(Label::primary(span.id()?, world.range(span)?))
}

impl<'a> codespan_reporting::files::Files<'a> for SystemWorld {
    type FileId = FileId;
    type Name = String;
    type Source = Source;

    fn name(&'a self, id: FileId) -> CodespanResult<Self::Name> {
        let vpath = id.vpath();
        Ok(if let Some(package) = id.package() {
            format!("{package}{}", vpath.as_rooted_path().display())
        } else {
            // Try to express the path relative to the working directory.
            vpath
                .resolve(self.root())
                .and_then(|abs| pathdiff::diff_paths(abs, self.workdir()))
                .as_deref()
                .unwrap_or_else(|| vpath.as_rootless_path())
                .to_string_lossy()
                .into()
        })
    }

    fn source(&'a self, id: FileId) -> CodespanResult<Self::Source> {
        Ok(self.lookup(id))
    }

    fn line_index(&'a self, id: FileId, given: usize) -> CodespanResult<usize> {
        let source = self.lookup(id);
        source
            .byte_to_line(given)
            .ok_or_else(|| CodespanError::IndexTooLarge {
                given,
                max: source.len_bytes(),
            })
    }

    fn line_range(
        &'a self,
        id: FileId,
        given: usize,
    ) -> CodespanResult<std::ops::Range<usize>> {
        let source = self.lookup(id);
        source
            .line_to_range(given)
            .ok_or_else(|| CodespanError::LineTooLarge { given, max: source.len_lines() })
    }

    fn column_number(
        &'a self,
        id: FileId,
        _: usize,
        given: usize,
    ) -> CodespanResult<usize> {
        let source = self.lookup(id);
        source.byte_to_column(given).ok_or_else(|| {
            let max = source.len_bytes();
            if given <= max {
                CodespanError::InvalidCharBoundary { given }
            } else {
                CodespanError::IndexTooLarge { given, max }
            }
        })
    }
}
