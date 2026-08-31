use serde::{Deserialize, Serialize};

use crate::chat_file::ChatFileRef;

// ---------------------------------------------------------------------------
// A. Preview requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct StartPreviewRequest {
    /// Legacy device-path identity. Retained for the frontend migration window;
    /// used when `file` is absent. Remove once all callers send `file`.
    pub file_path: String,
    #[serde(default)]
    pub workspace: Option<String>,
    /// Preferred identity: a [`ChatFileRef`] the backend resolves to an absolute
    /// path (keeps pe→path resolution server-side). When present it takes
    /// precedence over `file_path`.
    #[serde(default)]
    pub file: Option<ChatFileRef>,
}

/// `POST /api/{word|excel|ppt}-preview/refresh` body — force the running watch
/// server to re-read this document from disk.
///
/// Same shape as [`StartPreviewRequest`]: the caller already holds that payload
/// for the tab it is refreshing, and the backend has to resolve the identity to
/// the same absolute path `start` keyed its session on.
#[derive(Debug, Clone, Deserialize)]
pub struct RefreshPreviewRequest {
    /// Legacy device-path identity. Retained for the frontend migration window;
    /// used when `file` is absent.
    pub file_path: String,
    #[serde(default)]
    pub workspace: Option<String>,
    /// Preferred identity: a [`ChatFileRef`] the backend resolves to an absolute
    /// path. When present it takes precedence over `file_path`.
    #[serde(default)]
    pub file: Option<ChatFileRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StopPreviewRequest {
    /// Legacy device-path identity. Retained for the frontend migration window;
    /// used when `file` is absent. Remove once all callers send `file`.
    pub file_path: String,
    /// Preferred identity: a [`ChatFileRef`] the backend resolves to an absolute
    /// path (keeps pe→path resolution server-side). When present it takes
    /// precedence over `file_path`. Must resolve to the same path `start` used,
    /// so the watch session is stopped by the same key.
    #[serde(default)]
    pub file: Option<ChatFileRef>,
}

// ---------------------------------------------------------------------------
// B. Preview responses & events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PreviewUrlResponse {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `POST /api/{word|excel|ppt}-preview/refresh` response.
///
/// A failed refresh is reported as `ok: false` with a code rather than an HTTP
/// error: the watch server keeps serving the current document when a reload
/// fails, so the tab the user is looking at is still intact and the frontend only
/// needs to say the refresh did not take.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RefreshPreviewResponse {
    pub ok: bool,
    /// Stable code for the frontend's existing office-error copy; absent on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewState {
    Starting,
    Installing,
    Ready,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PreviewStatusEvent {
    pub state: PreviewState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// C. Document conversion
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConversionTarget {
    #[serde(rename = "markdown")]
    Markdown,
    #[serde(rename = "excel-json")]
    ExcelJson,
    #[serde(rename = "ppt-json")]
    PptJson,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DocumentConversionRequest {
    pub file_path: String,
    pub to: ConversionTarget,
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentConversionResponse {
    pub to: String,
    pub result: ConversionResultDto,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversionResultDto {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// D. Excel conversion data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExcelWorkbookData {
    pub sheets: Vec<ExcelSheetData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExcelSheetData {
    pub name: String,
    pub data: Vec<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merges: Option<Vec<CellRange>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ExcelSheetImage>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CellRange {
    pub s: CellCoord,
    pub e: CellCoord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CellCoord {
    pub r: usize,
    pub c: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExcelSheetImage {
    pub row: usize,
    pub col: usize,
    pub src: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
}

// ---------------------------------------------------------------------------
// E. PPT conversion data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PptJsonData {
    pub slides: Vec<PptSlideData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PptSlideData {
    pub slide_number: usize,
    pub content: serde_json::Value,
}

// ---------------------------------------------------------------------------
// F. Native presentation studio
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct PresentationFileRequest {
    #[serde(default)]
    pub file_path: String,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub file: Option<ChatFileRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PresentationRenderRequest {
    #[serde(flatten)]
    pub source: PresentationFileRequest,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PresentationAssetImportRequest {
    pub deck: PresentationFileRequest,
    pub source_file: ChatFileRef,
    pub asset_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresentationAssetImportResponse {
    pub asset_path: String,
    pub byte_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PresentationCatalogResponse {
    pub version: String,
    pub hash: String,
    pub themes: Vec<PresentationCatalogTheme>,
    pub layouts: Vec<PresentationCatalogLayout>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PresentationCatalogTheme {
    pub id: String,
    pub label: String,
    pub tokens: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PresentationCatalogLayout {
    pub id: String,
    pub role: String,
    pub label: String,
    pub slots: Vec<PresentationCatalogSlot>,
    pub controls: Vec<PresentationCatalogControl>,
    pub overflow_strategy: String,
    #[serde(default)]
    pub alternative_layout_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PresentationCatalogSlot {
    pub id: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub accepts: Vec<String>,
    #[serde(default)]
    pub required: bool,
    pub max_length: Option<u64>,
    pub max_items: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PresentationCatalogControl {
    pub id: String,
    #[serde(rename = "type")]
    pub control_type: String,
    pub label: String,
    pub default_value: serde_json::Value,
    #[serde(default)]
    pub options: Vec<String>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PresentationDiagnostic {
    pub severity: String,
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "slideId")]
    pub slide_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "blockId")]
    pub block_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PresentationValidationResponse {
    pub valid: bool,
    pub diagnostics: Vec<PresentationDiagnostic>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PresentationRenderStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PresentationRenderJob {
    pub job_id: String,
    pub revision: u64,
    pub status: PresentationRenderStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- A. StartPreviewRequest / StopPreviewRequest --------------------------

    #[test]
    fn start_preview_request_deserialize() {
        let raw = json!({"file_path": "/path/to/doc.docx", "workspace": "/tmp/ws"});
        let req: StartPreviewRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.file_path, "/path/to/doc.docx");
        assert_eq!(req.workspace.as_deref(), Some("/tmp/ws"));
    }

    #[test]
    fn start_preview_request_missing_file_path() {
        let raw = json!({});
        assert!(serde_json::from_value::<StartPreviewRequest>(raw).is_err());
    }

    #[test]
    fn stop_preview_request_deserialize() {
        let raw = json!({"file_path": "/path/to/doc.docx"});
        let req: StopPreviewRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.file_path, "/path/to/doc.docx");
    }

    #[test]
    fn start_preview_request_workspace_optional() {
        let raw = json!({"file_path": "/path/to/doc.docx"});
        let req: StartPreviewRequest = serde_json::from_value(raw).unwrap();
        assert!(req.workspace.is_none());
    }

    // -- B. PreviewUrlResponse ------------------------------------------------

    #[test]
    fn preview_url_response_success() {
        let resp = PreviewUrlResponse {
            url: "http://localhost:3000/preview".into(),
            error: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["url"], "http://localhost:3000/preview");
        assert!(json.get("error").is_none());
    }

    #[test]
    fn preview_url_response_error() {
        let resp = PreviewUrlResponse {
            url: String::new(),
            error: Some("officecli not found".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["url"], "");
        assert_eq!(json["error"], "officecli not found");
    }

    #[test]
    fn preview_url_response_roundtrip() {
        let resp = PreviewUrlResponse {
            url: "http://localhost:8080".into(),
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: PreviewUrlResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    // -- B2. PreviewState / PreviewStatusEvent --------------------------------

    #[test]
    fn preview_state_serialize_all_variants() {
        let cases = [
            (PreviewState::Starting, "starting"),
            (PreviewState::Installing, "installing"),
            (PreviewState::Ready, "ready"),
            (PreviewState::Error, "error"),
        ];
        for (state, expected) in cases {
            let json = serde_json::to_value(state).unwrap();
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn preview_state_deserialize_all_variants() {
        let cases = [
            ("starting", PreviewState::Starting),
            ("installing", PreviewState::Installing),
            ("ready", PreviewState::Ready),
            ("error", PreviewState::Error),
        ];
        for (input, expected) in cases {
            let parsed: PreviewState = serde_json::from_value(json!(input)).unwrap();
            assert_eq!(parsed, expected);
        }
    }

    #[test]
    fn preview_state_invalid() {
        assert!(serde_json::from_value::<PreviewState>(json!("unknown")).is_err());
    }

    #[test]
    fn preview_status_event_serialize() {
        let event = PreviewStatusEvent {
            state: PreviewState::Ready,
            message: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["state"], "ready");
        assert!(json.get("message").is_none());
    }

    #[test]
    fn preview_status_event_with_message() {
        let event = PreviewStatusEvent {
            state: PreviewState::Error,
            message: Some("port timeout".into()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["state"], "error");
        assert_eq!(json["message"], "port timeout");
    }

    #[test]
    fn preview_status_event_roundtrip() {
        let event = PreviewStatusEvent {
            state: PreviewState::Installing,
            message: Some("installing officecli...".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PreviewStatusEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    // -- C. Document conversion -----------------------------------------------

    #[test]
    fn conversion_target_serialize() {
        let cases = [
            (ConversionTarget::Markdown, "markdown"),
            (ConversionTarget::ExcelJson, "excel-json"),
            (ConversionTarget::PptJson, "ppt-json"),
        ];
        for (target, expected) in cases {
            let json = serde_json::to_value(target).unwrap();
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn conversion_target_deserialize() {
        let cases = [
            ("markdown", ConversionTarget::Markdown),
            ("excel-json", ConversionTarget::ExcelJson),
            ("ppt-json", ConversionTarget::PptJson),
        ];
        for (input, expected) in cases {
            let parsed: ConversionTarget = serde_json::from_value(json!(input)).unwrap();
            assert_eq!(parsed, expected);
        }
    }

    #[test]
    fn conversion_target_invalid() {
        assert!(serde_json::from_value::<ConversionTarget>(json!("invalid")).is_err());
    }

    #[test]
    fn document_conversion_request_deserialize() {
        let raw = json!({
            "file_path": "/sheet.xlsx",
            "to": "excel-json",
            "workspace": "/tmp/ws"
        });
        let req: DocumentConversionRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.file_path, "/sheet.xlsx");
        assert_eq!(req.to, ConversionTarget::ExcelJson);
        assert_eq!(req.workspace.as_deref(), Some("/tmp/ws"));
    }

    #[test]
    fn document_conversion_request_missing_to() {
        let raw = json!({"file_path": "/a.docx"});
        assert!(serde_json::from_value::<DocumentConversionRequest>(raw).is_err());
    }

    #[test]
    fn document_conversion_request_invalid_to() {
        let raw = json!({"file_path": "/a.docx", "to": "pdf"});
        assert!(serde_json::from_value::<DocumentConversionRequest>(raw).is_err());
    }

    #[test]
    fn document_conversion_request_workspace_optional() {
        let raw = json!({"file_path": "/sheet.xlsx", "to": "excel-json"});
        let req: DocumentConversionRequest = serde_json::from_value(raw).unwrap();
        assert!(req.workspace.is_none());
    }

    #[test]
    fn document_conversion_response_success() {
        let resp = DocumentConversionResponse {
            to: "excel-json".into(),
            result: ConversionResultDto {
                success: true,
                data: Some(json!({"sheets": []})),
                error: None,
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["to"], "excel-json");
        assert_eq!(json["result"]["success"], true);
        assert!(json["result"].get("error").is_none());
    }

    #[test]
    fn document_conversion_response_failure() {
        let resp = DocumentConversionResponse {
            to: "markdown".into(),
            result: ConversionResultDto {
                success: false,
                data: None,
                error: Some("pandoc not installed".into()),
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["result"]["success"], false);
        assert_eq!(json["result"]["error"], "pandoc not installed");
        assert!(json["result"].get("data").is_none());
    }

    #[test]
    fn document_conversion_response_roundtrip() {
        let resp = DocumentConversionResponse {
            to: "ppt-json".into(),
            result: ConversionResultDto {
                success: true,
                data: Some(json!({"slides": [{"slideNumber": 1, "content": {}}]})),
                error: None,
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DocumentConversionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    // -- D. Excel data model --------------------------------------------------

    #[test]
    fn excel_workbook_data_serialize() {
        let wb = ExcelWorkbookData {
            sheets: vec![ExcelSheetData {
                name: "Sheet1".into(),
                data: vec![vec![json!("Name"), json!("Age")], vec![json!("Alice"), json!(30)]],
                merges: None,
                images: None,
            }],
        };
        let json = serde_json::to_value(&wb).unwrap();
        assert_eq!(json["sheets"][0]["name"], "Sheet1");
        assert_eq!(json["sheets"][0]["data"][0][0], "Name");
        assert_eq!(json["sheets"][0]["data"][1][1], 30);
        assert!(json["sheets"][0].get("merges").is_none());
        assert!(json["sheets"][0].get("images").is_none());
    }

    #[test]
    fn excel_sheet_with_merges() {
        let sheet = ExcelSheetData {
            name: "Merged".into(),
            data: vec![vec![json!("A")]],
            merges: Some(vec![CellRange {
                s: CellCoord { r: 0, c: 0 },
                e: CellCoord { r: 1, c: 2 },
            }]),
            images: None,
        };
        let json = serde_json::to_value(&sheet).unwrap();
        assert_eq!(json["merges"][0]["s"]["r"], 0);
        assert_eq!(json["merges"][0]["s"]["c"], 0);
        assert_eq!(json["merges"][0]["e"]["r"], 1);
        assert_eq!(json["merges"][0]["e"]["c"], 2);
    }

    #[test]
    fn excel_sheet_with_images() {
        let sheet = ExcelSheetData {
            name: "Images".into(),
            data: vec![],
            merges: None,
            images: Some(vec![ExcelSheetImage {
                row: 0,
                col: 1,
                src: "data:image/png;base64,abc".into(),
                width: Some(200),
                height: Some(100),
                alt: Some("logo".into()),
            }]),
        };
        let json = serde_json::to_value(&sheet).unwrap();
        let img = &json["images"][0];
        assert_eq!(img["row"], 0);
        assert_eq!(img["col"], 1);
        assert_eq!(img["src"], "data:image/png;base64,abc");
        assert_eq!(img["width"], 200);
        assert_eq!(img["height"], 100);
        assert_eq!(img["alt"], "logo");
    }

    #[test]
    fn excel_sheet_image_minimal() {
        let img = ExcelSheetImage {
            row: 5,
            col: 3,
            src: "data:image/jpeg;base64,xyz".into(),
            width: None,
            height: None,
            alt: None,
        };
        let json = serde_json::to_value(&img).unwrap();
        assert_eq!(json["row"], 5);
        assert_eq!(json["col"], 3);
        assert!(json.get("width").is_none());
        assert!(json.get("height").is_none());
        assert!(json.get("alt").is_none());
    }

    #[test]
    fn cell_range_roundtrip() {
        let range = CellRange {
            s: CellCoord { r: 0, c: 0 },
            e: CellCoord { r: 3, c: 5 },
        };
        let json = serde_json::to_string(&range).unwrap();
        let parsed: CellRange = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, range);
    }

    #[test]
    fn excel_workbook_roundtrip() {
        let wb = ExcelWorkbookData {
            sheets: vec![
                ExcelSheetData {
                    name: "S1".into(),
                    data: vec![vec![json!(1), json!(2)]],
                    merges: Some(vec![CellRange {
                        s: CellCoord { r: 0, c: 0 },
                        e: CellCoord { r: 0, c: 1 },
                    }]),
                    images: Some(vec![ExcelSheetImage {
                        row: 0,
                        col: 0,
                        src: "data:x".into(),
                        width: Some(50),
                        height: None,
                        alt: None,
                    }]),
                },
                ExcelSheetData {
                    name: "S2".into(),
                    data: vec![],
                    merges: None,
                    images: None,
                },
            ],
        };
        let json = serde_json::to_string(&wb).unwrap();
        let parsed: ExcelWorkbookData = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, wb);
    }

    // -- E. PPT data model ----------------------------------------------------

    #[test]
    fn ppt_json_data_serialize() {
        let ppt = PptJsonData {
            slides: vec![PptSlideData {
                slide_number: 1,
                content: json!({"title": "Intro"}),
            }],
            raw: None,
        };
        let json = serde_json::to_value(&ppt).unwrap();
        assert_eq!(json["slides"][0]["slide_number"], 1);
        assert_eq!(json["slides"][0]["content"]["title"], "Intro");
        assert!(json.get("raw").is_none());
    }

    #[test]
    fn ppt_json_data_with_raw() {
        let ppt = PptJsonData {
            slides: vec![],
            raw: Some(json!({"format": "pptx", "version": 1})),
        };
        let json = serde_json::to_value(&ppt).unwrap();
        assert_eq!(json["raw"]["format"], "pptx");
    }

    #[test]
    fn ppt_json_data_roundtrip() {
        let ppt = PptJsonData {
            slides: vec![
                PptSlideData {
                    slide_number: 1,
                    content: json!({"title": "A"}),
                },
                PptSlideData {
                    slide_number: 2,
                    content: json!({"body": "text"}),
                },
            ],
            raw: Some(json!({"meta": true})),
        };
        let json = serde_json::to_string(&ppt).unwrap();
        let parsed: PptJsonData = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ppt);
    }

    #[test]
    fn ppt_slide_data_serialize() {
        let slide = PptSlideData {
            slide_number: 3,
            content: json!({"elements": []}),
        };
        let json = serde_json::to_value(&slide).unwrap();
        assert_eq!(json["slide_number"], 3);
    }

    #[test]
    fn presentation_catalog_uses_officecli_camel_case_contract() {
        let raw = json!({
            "version": "1.0.0",
            "hash": "abc",
            "themes": [],
            "layouts": [{
                "id": "cover",
                "role": "cover",
                "label": "Cover",
                "slots": [{
                    "id": "title", "x": 0.1, "y": 0.1, "width": 0.8, "height": 0.2,
                    "accepts": ["text"], "required": true, "maxLength": 120
                }],
                "controls": [{"id": "density", "type": "range", "label": "Density", "defaultValue": 2}],
                "overflowStrategy": "diagnose",
                "alternativeLayoutIds": ["statement"]
            }]
        });
        let catalog: PresentationCatalogResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(catalog.layouts[0].slots[0].max_length, Some(120));
        assert_eq!(catalog.layouts[0].controls[0].control_type, "range");
        assert_eq!(catalog.layouts[0].alternative_layout_ids, ["statement"]);
    }

    #[test]
    fn presentation_diagnostic_accepts_officecli_camel_case_targets() {
        let diagnostic: PresentationDiagnostic = serde_json::from_value(json!({
            "severity": "error",
            "code": "DECK_SLOT_REQUIRED",
            "message": "Required slot is empty",
            "slideId": "slide-1",
            "blockId": "block-1"
        }))
        .unwrap();
        assert_eq!(diagnostic.slide_id.as_deref(), Some("slide-1"));
        assert_eq!(diagnostic.block_id.as_deref(), Some("block-1"));
        let response = serde_json::to_value(diagnostic).unwrap();
        assert_eq!(response["slide_id"], "slide-1");
        assert_eq!(response["block_id"], "block-1");
    }

    #[test]
    fn presentation_render_request_matches_flattened_workmate_payload() {
        let request: PresentationRenderRequest = serde_json::from_value(json!({
            "file_path": "",
            "file": {
                "kind": "project",
                "pe_id": "project-1",
                "relative_path": "decks/q1.workmate-deck.json"
            },
            "expected_revision": 7
        }))
        .unwrap();
        assert_eq!(request.expected_revision, 7);
        assert_eq!(request.source.file_path, "");
        assert!(matches!(
            request.source.file,
            Some(ChatFileRef::Project { ref pe_id, ref relative_path })
                if pe_id == "project-1" && relative_path == "decks/q1.workmate-deck.json"
        ));
    }

    #[test]
    fn presentation_asset_import_keeps_deck_and_source_scopes_separate() {
        let request: PresentationAssetImportRequest = serde_json::from_value(json!({
            "deck": { "file_path": "/workspace/q1.workmate-deck.json" },
            "source_file": { "kind": "local", "path": "/images/hero.png" },
            "asset_id": "hero"
        }))
        .unwrap();
        assert_eq!(request.deck.file_path, "/workspace/q1.workmate-deck.json");
        assert_eq!(request.asset_id, "hero");
        assert!(matches!(request.source_file, ChatFileRef::Local { ref path } if path == "/images/hero.png"));
    }

    #[test]
    fn presentation_job_serializes_renderer_status_contract() {
        let job = PresentationRenderJob {
            job_id: "job-1".into(),
            revision: 7,
            status: PresentationRenderStatus::Running,
            output_file: None,
            error_code: None,
        };
        let response = serde_json::to_value(job).unwrap();
        assert_eq!(response["job_id"], "job-1");
        assert_eq!(response["status"], "running");
        assert!(response.get("output_file").is_none());
    }
}
