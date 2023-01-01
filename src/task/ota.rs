use crate::error;
use crate::errors::*;
use crate::state::*;
use core::mem;
use core::ptr;
use embassy_time::{Duration, Timer};
use esp_idf_svc::http::client::{Configuration, EspHttpConnection};
use esp_idf_svc::ota::EspOta;
use esp_idf_sys::*;
use heapless::String;
use log::*;

const BUF_MAX: usize = 1024;

#[derive(Clone, Debug)]
pub struct FirmwareInfo {
    pub version: heapless::String<32>,
    pub date: heapless::String<16>,
    pub time: heapless::String<16>,
    pub description: heapless::String<128>,
}

pub async fn ota_task() {
    let mut subscriber = APPLICATION_EVENT_CHANNEL.subscriber().unwrap();

    loop {
        if let ApplicationStateChange::OTAUpdateRequest(url) = subscriber.next_message_pure().await
        {
            info!("processing OTA request for URL = {}", url);

            let publisher = APPLICATION_EVENT_CHANNEL.publisher().unwrap();

            // Notify all tasks that the OTA update started. These tasks are
            // expected to shutdown
            let data = ApplicationStateChange::OTAUpdateStarted;
            let _ = publisher.publish(data).await;
            Timer::after(Duration::from_secs(2)).await;
            perform_update("http://192.168.100.86/bin/firmware-0.1.0.bin");
        }
    }
}

// TODO: as of Dec 2022 there is no async http client implementation for ESP IDF.
// once an async implementation becomes available rework this code to become async
fn perform_update(firmware_url: &str) {
    info!("perform_update enter");
    let mut content_length: usize = 0;
    let mut ota_write_data: [u8; BUF_MAX] = [0; BUF_MAX];
    let mut firmware_update_ok = false;
    let mut invalid_fw_version: heapless::String<32> = String::new();
    let mut found_invalid_fw = false;

    let mut client = EspHttpConnection::new(&Configuration {
        buffer_size: Some(BUF_MAX),
        ..Default::default()
    })
    .expect("creation of EspHttpConnection should have worked");

    info!("EspHttpConnection created");
    let _resp = client.initiate_request(embedded_svc::http::Method::Get, firmware_url, &[]);

    info!("after client.initiate_request()");

    if let Err(err) = client.initiate_response() {
        error!("Error initiate response {}", err);
        return;
    }

    if let Some(len) = client.header("Content-Length") {
        content_length = len.parse().unwrap();
    } else {
        error!("reading content length for firmware update http request failed");
    }

    info!("Content-length: {:?}", content_length);

    info!(">>>>>>>>>>>>>>>> initiating OTA update");

    let update_partition: esp_partition_t =
        unsafe { *esp_ota_get_next_update_partition(ptr::null()) };
    let partition_label =
        std::str::from_utf8(unsafe { std::mem::transmute(&update_partition.label as &[i8]) })
            .unwrap()
            .trim_matches(char::from(0));
    info!(
        "Writing to partition {} subtype {:#4x} size {:#10x} at offset {:#10x}",
        partition_label, update_partition.subtype, update_partition.size, update_partition.address
    );

    let mut ota = EspOta::new().expect("EspOta::new should have been successfull");

    let boot_slot = ota.get_boot_slot().unwrap();
    info!("boot slot = {:?}", boot_slot);

    let run_slot = ota.get_running_slot().unwrap();
    info!("run slot = {:?}", run_slot);

    let update_slot = ota.get_update_slot().unwrap();
    info!("update slot = {:?}", update_slot);

    let invalid_slot = ota.get_last_invalid_slot().unwrap();
    if invalid_slot.is_none() {
        info!("no invalid slot found");
    } else {
        info!("last invalid slot = {:?}", invalid_slot);
        let slot = invalid_slot.unwrap();
        if slot.firmware.is_some() {
            let fw = slot.firmware.unwrap();
            if let Err(err) = invalid_fw_version.push_str(fw.version.as_str()) {
                error!("failed to load invalid fw version {:?}", err);
            }
            found_invalid_fw = true;
        }
    }
    info!("initiating ota update...");
    let ota_update = ota
        .initiate_update()
        .expect("initiate ota update should have worked");
    info!("...ota update started");

    let mut bytes_read_total = 0;
    let mut image_header_was_checked = false;

    loop {
        esp_idf_hal::delay::FreeRtos::delay_ms(20);
        let data_read = match client.read(&mut ota_write_data) {
            Ok(n) => n,
            Err(err) => {
                error!("ERROR reading firmware batch {:?}", err);
                break;
            }
        };
        //info!("Bytes read: {}", data_read);

        if !image_header_was_checked
            && data_read
                > mem::size_of::<esp_image_header_t>()
                    + mem::size_of::<esp_image_segment_header_t>()
                    + mem::size_of::<esp_app_desc_t>()
        {
            let download_fw_info = get_firmware_info_from_download(&ota_write_data).unwrap();
            info!("Firmware info = {:?}", download_fw_info);

            if found_invalid_fw {
                if invalid_fw_version == download_fw_info.version {
                    info!("New FW has same version as invalide firmware slot. Stopping update");
                    break;
                }
            }
            image_header_was_checked = true;
        }

        bytes_read_total += data_read;

        if !ota_write_data.is_empty() {
            match ota_update.write(&ota_write_data) {
                Ok(_) => {}
                Err(err) => {
                    error!("ERROR failed to write update with: {:?}", err);
                    break;
                }
            }
        } else {
            error!("ERROR firmware image with zero length");
            break;
        }

        if ota_write_data.len() > data_read {
            break;
        }
    }

    if bytes_read_total == content_length {
        firmware_update_ok = true;
    }

    if firmware_update_ok {
        info!("Trying to set update to complete");
        match ota_update.complete() {
            Ok(_) => {
                info!("OTA completed firmware update");
            }
            Err(err) => {
                error!("OTA update failed. esp_ota_end failed {:?}", err);
            }
        }
    } else {
        ota_update.abort().unwrap();
        error!("ERROR firmware update failed");
    };

    esp_idf_hal::delay::FreeRtos::delay_ms(1000);
    info!("restarting device after firmware update");

    unsafe {
        esp_idf_sys::esp_restart();
    }
}

fn get_firmware_info_from_download(data: &[u8]) -> Result<FirmwareInfo, InitError> {
    let offset =
        mem::size_of::<esp_image_header_t>() + mem::size_of::<esp_image_segment_header_t>();
    let (_head, body, _tail) = unsafe { &data[offset..].align_to::<esp_app_desc_t>() };

    let app_desc: &esp_app_desc_t = &body[0];

    let version = std::str::from_utf8(unsafe { std::mem::transmute(&app_desc.version as &[i8]) })
        .unwrap()
        .trim_matches(char::from(0));

    let project_name =
        std::str::from_utf8(unsafe { std::mem::transmute(&app_desc.project_name as &[i8]) })
            .unwrap()
            .trim_matches(char::from(0));

    let date = std::str::from_utf8(unsafe { std::mem::transmute(&app_desc.date as &[i8]) })
        .unwrap()
        .trim_matches(char::from(0));

    let time = std::str::from_utf8(unsafe { std::mem::transmute(&app_desc.time as &[i8]) })
        .unwrap()
        .trim_matches(char::from(0));

    Ok(FirmwareInfo {
        version: String::from(version),
        date: String::from(date),
        time: String::from(time),
        description: String::from(project_name),
    })
}
