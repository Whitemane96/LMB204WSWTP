use axum::{
    extract::{Multipart, Json, State},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router, body::Body,
};
use dotext::*;
use std::io::{Read, Write, Cursor};
use std::fs;
use std::fs::File;
use std::net::SocketAddr;
use std::sync::Arc;
use dotenvy::dotenv;
use serde::{Deserialize, Serialize};
use async_openai::{
    types::{ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs, 
           CreateChatCompletionRequestArgs, ChatCompletionResponseFormat, ChatCompletionResponseFormatType},
    Client,
};
use zip::{ZipArchive, ZipWriter, write::FileOptions};
use handlebars::Handlebars;
use dashmap::DashMap;
use uuid::Uuid;
use chrono::{NaiveDate, Utc, FixedOffset};
use serde::de::{self, Deserializer};

#[derive(Serialize, Deserialize, Debug, Clone)]
struct RawData {
    pub full_name: String,
    pub gender: String,
    pub company: String,
    pub working_hours: u32,
    pub pay_rate: f64,
    #[serde(default)]
    pub working_days_per_week: u32,
    #[serde(default)]
    pub total_weeks_violated: u32,
    #[serde(default)]
    pub meal_violations_per_week: u32,
    #[serde(default)]
    pub rest_violations_per_week: u32,
    pub pay_period: Option<String>,
    pub demand_amount: Option<f64>,
    pub atty_contact_date: Option<String>,
    pub custom_sentence1: Option<String>, 
    pub custom_sentence2: Option<String>,
    #[serde(default, deserialize_with = "deserialize_number_from_string")]
    pub penalty_days: Option<u32>
}

type SharedState = Arc<DashMap<String, RawData>>;

fn deserialize_number_from_string<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    let v: serde_json::Value = Deserialize::deserialize(deserializer)?;
    match v {
        serde_json::Value::Number(n) => Ok(n.as_u64().map(|x| x as u32)),
        serde_json::Value::String(s) => s.parse::<u32>().map(Some).map_err(de::Error::custom),
        _ => Ok(None),
    }
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    let state: SharedState = Arc::new(DashMap::new());

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/upload", post(upload_handler))
        .route("/chat", post(chat_handler))
        .route("/generate", post(generate_handler))
        .route("/clear", post(clear_session_handler))
        .with_state(state);

    //Local Setup
    // let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    // println!("Server running at http://{}", addr);
    // let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    // axum::serve(listener, app).await.unwrap();

    //Deploy
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("0.0.0.0:{}", port);
    println!("Server listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn clear_session_handler(
    State(state): State<SharedState>,
    Json(req): Json<ChatRequest>,
) -> impl IntoResponse {
    state.remove(&req.session_id);

    let path = format!("./{}.docx", req.session_id);
    let _ = fs::remove_file(path);

    axum::http::StatusCode::OK
}

async fn index_handler() -> Html<String> {
    std::fs::read_to_string("index.html").map(Html).unwrap_or(Html("index.html not found".into()))
}

async fn upload_handler(State(state): State<SharedState>, mut multipart: Multipart) -> impl IntoResponse {
    let session_id = Uuid::new_v4().to_string();
    while let Some(field) = multipart.next_field().await.unwrap() {
        if field.name() == Some("file") {
            let data = field.bytes().await.unwrap();
            let path = format!("./{}.docx", session_id);
            std::fs::write(&path, &data).unwrap();

            let text = read_docx(&path);
            let extracted = extract_data(&text, None).await; 
            
            state.insert(session_id.clone(), extracted.clone());
            return Json(serde_json::json!({ "session_id": session_id, "data": extracted })).into_response();
        }
    }
    (axum::http::StatusCode::BAD_REQUEST, "No file").into_response()
}

#[derive(Deserialize)]
struct ChatRequest {
    session_id: String,
    message: String,
}

async fn chat_handler(State(state): State<SharedState>, Json(req): Json<ChatRequest>) -> impl IntoResponse {
    let current_data = state.get(&req.session_id).map(|r| r.value().clone());
    let updated = extract_data(&req.message, current_data).await;
    state.insert(req.session_id.clone(), updated.clone());
    Json(updated)
}

// Helper function to round up to 2 decimal places
    fn round_up(value: f64) -> f64 {
        (value * 100.0).ceil() / 100.0
    }

async fn generate_handler(Json(data): Json<RawData>) -> Response {
    let first_name = data.full_name.split_whitespace().next().unwrap_or("").to_string();
    let last_name = data.full_name.split_whitespace().last().unwrap_or("").to_string();

    let offset = FixedOffset::west_opt(8 * 3600).unwrap();
    let ca_today = Utc::now().with_timezone(&offset);
    let today_date_str = ca_today.format("%B %e, %Y").to_string();

    let formatted_atty_date = if let Some(ref date_str) = data.atty_contact_date {
        if let Ok(parsed_date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
            parsed_date.format("%A, %B %e, %Y").to_string()
        } else {
            date_str.clone()
        }
    } else {
        "N/A".to_string()
    };

    let (title, pronoun, possessive, verb) = match data.gender.to_lowercase().as_str() {
        "male" => ("Mr.", "he", "his", "is"),
        "female" => ("Ms.", "she", "her", "is"),
        _ => ("Mx.", "they", "their", "are"),
    };

    // Calculations
    let weeks = data.total_weeks_violated;
    let meal_shifts = weeks * data.meal_violations_per_week;
    let rest_shifts = weeks * data.rest_violations_per_week;

    let meal_total = round_up(meal_shifts as f64 * data.pay_rate);
    let rest_total = round_up(rest_shifts as f64 * data.pay_rate);

    let penalty_days = data.penalty_days.unwrap_or(0).min(30); // Capped at 30 days per CA law

    let total_per_day = round_up(data.pay_rate * data.working_hours as f64);
    let total_wtp = round_up(penalty_days as f64 * total_per_day);

    let num_violations = match data.pay_period.as_deref().map(|s| s.to_lowercase()).as_deref() {
        Some("weekly") => weeks,
        Some("biweekly") => weeks / 2,
        _ => 0,
    };

    let violations_total = round_up(if num_violations > 0 { 100.0 + ((num_violations - 1) as f64 * 200.0) } else { 0.0 });

    let penalties_sum = meal_total + rest_total;

    let violations_percent = round_up(penalties_sum * 0.25);

    let wage_total = round_up(if num_violations > 0 { 50.0 + ((num_violations - 1) as f64 * 100.0) } else { 0.0 });
    
    let total_sum = round_up(penalties_sum + violations_total + violations_percent + wage_total + total_wtp);

    let hb_data = serde_json::json!({
        "title": title,
        "firstname": first_name,
        "lastname": last_name,
        "company": data.company,
        "today_date": today_date_str,
        "custom_sentence1": data.custom_sentence1.as_deref().unwrap_or(""),
        "custom_sentence2": data.custom_sentence2.as_deref().unwrap_or(""),

        "pronoun": pronoun,
        "possessive": possessive,
        "verb": verb,

        "workingdays": data.working_days_per_week,
        "workinghours": data.working_hours,
        "payrate": data.pay_rate,

        "num_violations": num_violations,
        "mealbreakshifts": meal_shifts,
        "restbreakshifts": rest_shifts,

        "mealbreak_total": format!("{:.2}", meal_total),
        "restbreak_total": format!("{:.2}", rest_total),
        "penalties_sum": format!("{:.2}", penalties_sum),
        "violations_total": format!("{:.2}", violations_total),
        "violations_percent": format!("{:.2}", violations_percent),
        
        "wage_total": format!("{:.2}", wage_total),
        "total_sum": format!("{:.2}", total_sum),

        "atty_contact_date": formatted_atty_date,
        "demand_amount": format!("{:.2}", data.demand_amount.unwrap_or(0.0) as f64),

        "penalty_days": penalty_days,
        "total_per_day": format!("{:.2}", total_per_day),
        "total_wtp": format!("{:.2}", total_wtp),
    });

    let hb = Handlebars::new();
    let file = File::open("Template.docx").expect("Template.docx not found");
    let mut archive = ZipArchive::new(file).unwrap();
    let mut out_buffer = Vec::new();

    {
        let mut writer = ZipWriter::new(Cursor::new(&mut out_buffer));
        for i in 0..archive.len() {
            let mut file = archive.by_index(i).unwrap();
            let name = file.name().to_string();
            writer.start_file(name.clone(), FileOptions::default().compression_method(file.compression())).unwrap();

            if name == "word/document.xml" {
                let mut content = String::new();
                file.read_to_string(&mut content).unwrap();
                let re = regex::Regex::new(r"\{\{([^}]+)\}\}").unwrap();
                let cleaned = re.replace_all(&content, |caps: &regex::Captures| {
                    regex::Regex::new(r"<[^>]*>").unwrap().replace_all(&caps[0], "").to_string()
                });
                let rendered = hb.render_template(&cleaned, &hb_data).unwrap_or(cleaned.into());
                writer.write_all(rendered.as_bytes()).unwrap();
            } else {
                let mut buffer = Vec::new();
                file.read_to_end(&mut buffer).unwrap();
                writer.write_all(&buffer).unwrap();
            }
        }
        writer.finish().unwrap();
    }

    Response::builder()
        .header("Content-Type", "application/vnd.openxmlformats-officedocument.wordprocessingml.document")
        .header("Content-Disposition", format!("attachment; filename=\"Letter_{}.docx\"", last_name))
        .body(Body::from(out_buffer)).unwrap()
}

async fn extract_data(input: &str, context: Option<RawData>) -> RawData {
    let client = Client::new();
    let context_prompt = match context {
        Some(c) => format!("The current extracted data is: {:?}. Update ONLY the fields mentioned. Keep everything else the same.", c),
        None => "Extract the legal data from the provided document text.".to_string(),
    };

    let request = CreateChatCompletionRequestArgs::default()
        .model("gpt-4o")
        .messages([
            ChatCompletionRequestSystemMessageArgs::default()
                .content(format!("{} Output a JSON object.

                Use right possessive articles based on gender, for male (his, he), female (her, she), and non-binary (their, they).
                If gender is non-binary, use 'Mx.' as the title.
                
                CUSTOM_SENTENCE1: Based on the reason, if you determine that breaks were denied every single shift, draft a sentence like this: '{{title}} {{lastname}} was denied {{possessive}} breaks during each of {{possessive}} shifts'. If you determine that break denial did not happen every day but specific shifts or, draft a sentence like: '{{title}} {{lastname}} was denied {{possessive}} breaks on multiple shifts'. Use placeholders {{title}}, {{lastname}}, and {{possessive}}. Do NOT include actual numbers here.

                CUSTOM_SENTENCE2: Determine which breaks were denied. Return 'rest and meal breaks in violation of California Law' if both were denied, 'meal breaks in violation of California Law' if only meal, or 'rest breaks in violation of California Law' if only rest.

                TIMEFRAME CALCULATION: Carefully calculate 'total_weeks_violated' based on the dates provided (e.g., 'Sept 2024 to Nov 2024' is ~13 weeks).

                Extract the number of days for 'penalty_days' (up to 30) after 'WTP' or 'Waiting Time Penalties' sentence in the file. Example: 'WTP: 10 days'. Return as an INTEGER (number), not a string. Example: 10.

                For demand_amount: Remove any symbols, keep only numbers with decimals if it has them.
                For atty_contact_date: Format as YYYY-MM-DD.
                For pay_period: Use 'weekly' or 'biweekly'.
                
                Keys: full_name, gender, company, working_hours, pay_rate, working_days_per_week, total_weeks_violated, meal_violations_per_week, rest_violations_per_week, pay_period, demand_amount, atty_contact_date, custom_sentence1, custom_sentence2, penalty_days.", context_prompt))
                .build().unwrap().into(),
            ChatCompletionRequestUserMessageArgs::default()
                .content(input).build().unwrap().into(),
        ])
        .response_format(ChatCompletionResponseFormat { r#type: ChatCompletionResponseFormatType::JsonObject })
        .build().unwrap();

    let response = client.chat().create(request).await.expect("OpenAI failed");
    let json_str = response.choices[0].message.content.as_ref().unwrap();
    serde_json::from_str(json_str).unwrap()
}

fn read_docx(path: &str) -> String {
    let mut file = Docx::open(path).expect("Read failed");
    let mut content = String::new();
    file.read_to_string(&mut content).unwrap();
    content
}