use crate::bus::protocol::MessageBody;

pub async fn run(service_name: &str) {
    super::service_client::send_elevated(MessageBody::StopService {
        service: service_name.to_string(),
    })
    .await;
}
