//! Durable attachment extraction for the mcpmem server.
//!
//! The worker leases one extraction job per workspace graph, decodes UTF-8
//! text files itself, and sends each rendered PDF page to a vision endpoint
//! for OCR. All OCR calls run outside the graph write transaction; the job
//! completes through the fenced core repository, which publishes every page
//! and segment row only when the lease and the attachment revision still
//! match the claim.
//!
//! Poppler's `pdfinfo` and `pdftoppm` are external host dependencies. The
//! crate does not bundle a PDF renderer.

pub mod ocr;
pub mod pdf;

pub use ocr::{
    DEFAULT_OCR_MODEL, DEFAULT_VISION_ENDPOINT, OcrConfig, OcrError, OcrProvider, PrimaryProvider,
    RejectedOcr, VisionOcr, VisionSettings, resolve_ocr, resolve_vision,
};
pub use pdf::{ExtractionError, ExtractionReport, ExtractionWorker};
