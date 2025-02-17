// https://github.com/moka-rs/moka/issues/31

use moka::future::Cache;

#[tokio::main]
async fn main() {
    let cache: Cache<_, String> = Cache::new(100);

    let data = "zero".to_string();
    {
        // Not 'static.
        let data_ref = &data;

        cache
            .get_or_insert_with(0, async {
                // This may become a dangling pointer.
                // The async block can be executed by a different thread so
                // the captured reference `data_ref` may outlive its value.
                data_ref.to_string()
            })
            .await;
    }

    println!("{:?}", data);
}
