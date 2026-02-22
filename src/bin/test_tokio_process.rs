#[tokio::main]
async fn main() {
    println!("Testing tokio::process...");
    match tokio::process::Command::new("echo")
        .arg("hello from tokio::process")
        .output()
        .await
    {
        Ok(out) => println!(
            "tokio::process ok: {:?}",
            String::from_utf8_lossy(&out.stdout)
        ),
        Err(e) => println!("tokio::process err: {}", e),
    }

    println!("Testing std::process in spawn_blocking...");
    match tokio::task::spawn_blocking(|| {
        std::process::Command::new("echo")
            .arg("hello from std::process")
            .output()
    })
    .await
    {
        Ok(Ok(out)) => println!(
            "std::process ok: {:?}",
            String::from_utf8_lossy(&out.stdout)
        ),
        Ok(Err(e)) => println!("std::process err: {}", e),
        Err(e) => println!("spawn_blocking err: {}", e),
    }
}
