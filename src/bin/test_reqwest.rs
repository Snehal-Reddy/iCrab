#[tokio::main]
async fn main() {
    println!("Testing reqwest TLS...");
    match reqwest::Client::builder().build() {
        Ok(client) => match client.get("https://api.telegram.org").send().await {
            Ok(res) => println!("reqwest ok, status: {}", res.status()),
            Err(e) => println!("reqwest send err: {}", e),
        },
        Err(e) => println!("reqwest builder err: {}", e),
    }
}
