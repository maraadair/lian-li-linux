use super::controller::WirelessController;
use super::convergence::AckSignal;
use super::DiscoveredDevice;
use super::{
    RF_217_CLOSE_WIFI, RF_DATA_SIZE, RF_REBOOT_LCD, RF_SELECT, RF_SELECTED_GROUP, RF_SEND_PIC,
};
use anyhow::{bail, ensure, Context, Result};
use lianli_transport::usb::USB_TIMEOUT;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tracing::debug;

const PIC_CHUNK_SIZE: usize = 220;
const PIC_TERMINATOR: u8 = 0xFF;
const MAX_AIO_IMAGE_BYTES: usize = 20_480;

struct PictureReservation(Arc<parking_lot::Mutex<Option<[u8; 6]>>>);

impl Drop for PictureReservation {
    fn drop(&mut self) {
        *self.0.lock() = None;
    }
}

fn validate_aio_image(device_type: u8, length: usize) -> Result<()> {
    ensure!(
        matches!(device_type, 10 | 11),
        "Wireless image upload requires HydroShift II Circle or Square"
    );
    ensure!(
        (1..=MAX_AIO_IMAGE_BYTES).contains(&length),
        "Wireless H2 image must contain 1 to {MAX_AIO_IMAGE_BYTES} bytes"
    );
    Ok(())
}

fn picture_packets(
    device: &DiscoveredDevice,
    master: &[u8; 6],
    channel: u8,
    sequence: u8,
    index: u8,
    data: &[u8],
) -> Result<[[u8; 64]; 4]> {
    ensure!(
        data.len() <= PIC_CHUNK_SIZE,
        "Wireless image chunk exceeds 220 bytes"
    );
    let mut rf = [0; RF_DATA_SIZE];
    rf[0] = RF_SELECT;
    rf[1] = RF_SEND_PIC;
    rf[2..8].copy_from_slice(&device.mac);
    rf[8..14].copy_from_slice(master);
    rf[14] = device.rx_type;
    rf[15] = channel;
    rf[17] = sequence;
    rf[18] = index;
    rf[19..19 + data.len()].copy_from_slice(data);
    Ok(std::array::from_fn(|part| {
        let mut packet = [0; 64];
        packet[..4].copy_from_slice(&[
            super::USB_CMD_SEND_RF,
            part as u8,
            device.channel,
            device.rx_type,
        ]);
        packet[4..].copy_from_slice(&rf[part * 60..(part + 1) * 60]);
        packet
    }))
}

impl WirelessController {
    pub fn selected_group(&self, mac: &[u8; 6]) -> Result<()> {
        let device = self
            .device_by_mac(mac)
            .context("device not found for selected group")?;
        let master_mac = *self.master_mac.lock();
        let master_ch = *self.master_channel.lock();
        let target_cmd_seq = self.bump_target_cmd_seq(mac, device.cmd_seq);

        let mut rf_data = vec![0u8; RF_DATA_SIZE];
        rf_data[0] = RF_SELECT;
        rf_data[1] = RF_SELECTED_GROUP;
        rf_data[2..8].copy_from_slice(&device.mac);
        rf_data[8..14].copy_from_slice(&master_mac);
        rf_data[14] = device.rx_type;
        rf_data[15] = master_ch;
        rf_data[17] = target_cmd_seq;

        self.enqueue_rf_command(
            &device,
            rf_data,
            AckSignal::CmdSeq(target_cmd_seq),
            "selected group".to_string(),
        )?;

        debug!("Selected group: {}", device.mac_str());
        Ok(())
    }

    pub fn reboot_lcd_group(&self, mac: &[u8; 6]) -> Result<()> {
        self.queue_lcd_reboot(mac, false)
    }

    /// Automatic recovery owns its retry budget and must not reboot restored playback later.
    pub fn reboot_lcd_group_once(&self, mac: &[u8; 6]) -> Result<()> {
        anyhow::ensure!(
            self.devices().iter().any(|device| device.mac == *mac),
            "LCD group is no longer bound to this controller"
        );
        self.queue_lcd_reboot(mac, true)
    }

    fn queue_lcd_reboot(&self, mac: &[u8; 6], once: bool) -> Result<()> {
        let device = self
            .device_by_mac(mac)
            .context("device not found for LCD reboot")?;
        let master_mac = *self.master_mac.lock();
        let master_ch = *self.master_channel.lock();
        let target_cmd_seq = self.bump_target_cmd_seq(mac, device.cmd_seq);

        let mut rf_data = vec![0u8; RF_DATA_SIZE];
        rf_data[0] = RF_SELECT;
        rf_data[1] = RF_REBOOT_LCD;
        rf_data[2..8].copy_from_slice(&device.mac);
        rf_data[8..14].copy_from_slice(&master_mac);
        rf_data[14] = device.rx_type;
        rf_data[15] = master_ch;
        rf_data[17] = target_cmd_seq;

        if once {
            self.enqueue_rf_command_with_retry_limit(
                &device,
                rf_data,
                AckSignal::CmdSeq(target_cmd_seq),
                "LCD reboot",
                0,
            )?;
        } else {
            self.enqueue_rf_command(
                &device,
                rf_data,
                AckSignal::CmdSeq(target_cmd_seq),
                "LCD reboot",
            )?;
        }

        debug!("LCD reboot: {}", device.mac_str());
        Ok(())
    }

    pub fn close_217_wifi(&self, mac: &[u8; 6], disable: bool) -> Result<()> {
        let device = self
            .device_by_mac(mac)
            .context("device not found for 217 wifi close")?;
        let master_mac = *self.master_mac.lock();
        let master_ch = *self.master_channel.lock();
        let target_cmd_seq = self.bump_target_cmd_seq(mac, device.cmd_seq);

        let mut rf_data = vec![0u8; RF_DATA_SIZE];
        rf_data[0] = RF_SELECT;
        rf_data[1] = RF_217_CLOSE_WIFI;
        rf_data[2..8].copy_from_slice(&device.mac);
        rf_data[8..14].copy_from_slice(&master_mac);
        rf_data[14] = device.rx_type;
        rf_data[15] = master_ch;
        rf_data[17] = target_cmd_seq;
        rf_data[20] = if disable { 1 } else { 0 };

        self.enqueue_rf_command(
            &device,
            rf_data,
            AckSignal::CmdSeq(target_cmd_seq),
            format!("217 wifi {}", if disable { "disable" } else { "enable" }),
        )?;

        debug!(
            "217 wifi {}: {}",
            if disable { "disabled" } else { "enabled" },
            device.mac_str()
        );
        Ok(())
    }

    pub fn send_aio_pic(&self, mac: &[u8; 6], image: &[u8], cancel: &AtomicBool) -> Result<()> {
        let device = self
            .device_by_mac(mac)
            .context("device not found for SendPic")?;
        validate_aio_image(device.device_type, image.len())?;
        ensure!(
            self.devices().iter().any(|d| d.mac == *mac),
            "Bind this H2 to the active wireless controller first"
        );
        let _reservation = {
            let _order = self.command_order.lock();
            let mut target = self.picture_target.lock();
            ensure!(target.is_none(), "Another wireless image upload is running");
            ensure!(
                self.binding_mac.lock().is_none(),
                "Wait for wireless binding to finish"
            );
            ensure!(
                self.pending_commands.as_ref().is_none_or(|queue| !queue
                    .lock()
                    .iter()
                    .any(|cmd| cmd.mac == *mac && matches!(cmd.ack, AckSignal::CmdSeq(_)))),
                "Wait for H2 control commands to finish before uploading"
            );
            *target = Some(*mac);
            PictureReservation(self.picture_target.clone())
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let master_mac = *self.master_mac.lock();
        let master_ch = *self.master_channel.lock();
        let tx = self
            .tx
            .as_ref()
            .context("TX device not available for SendPic")?;

        let total_chunks = image.len().div_ceil(PIC_CHUNK_SIZE);

        for chunk_idx in 0..total_chunks as u8 {
            self.check_picture_transfer(cancel, deadline)?;
            let start = chunk_idx as usize * PIC_CHUNK_SIZE;
            let end = (start + PIC_CHUNK_SIZE).min(image.len());
            let next_seq = self.send_pic_chunk(
                tx,
                &device,
                &master_mac,
                master_ch,
                chunk_idx,
                &image[start..end],
            )?;
            self.wait_pic_ack(&device.mac, next_seq, cancel, deadline)?;
        }

        let len = image.len() as u16;
        self.check_picture_transfer(cancel, deadline)?;
        let mut term_payload = [0u8; PIC_CHUNK_SIZE];
        term_payload[0] = (len >> 8) as u8;
        term_payload[1] = (len & 0xFF) as u8;
        let next_seq = self.send_pic_chunk(
            tx,
            &device,
            &master_mac,
            master_ch,
            PIC_TERMINATOR,
            &term_payload,
        )?;
        self.wait_pic_ack(&device.mac, next_seq, cancel, deadline)?;

        debug!(
            "SendPic: {} ({} chunks + terminator, {} bytes)",
            device.mac_str(),
            total_chunks,
            image.len(),
        );
        Ok(())
    }

    fn send_pic_chunk(
        &self,
        tx: &super::transport::SharedTransport,
        device: &DiscoveredDevice,
        master_mac: &[u8; 6],
        channel: u8,
        chunk_idx: u8,
        data: &[u8],
    ) -> Result<(u8, std::time::Instant)> {
        let mac = &device.mac;
        let current_seq = self.device_by_mac(mac).map(|d| d.cmd_seq).unwrap_or(0);
        let next_seq = self.bump_target_cmd_seq(mac, current_seq);

        let packets = picture_packets(device, master_mac, channel, next_seq, chunk_idx, data)?;
        let sent_at = std::time::Instant::now();
        super::transport::with_ready_transport(
            tx,
            &super::TX_IDS,
            "TX",
            &self.poll_stop,
            |handle| {
                anyhow::ensure!(
                    !self.poll_stop.load(Ordering::Acquire),
                    "wireless controller is stopping"
                );
                for packet in packets {
                    let written = handle.write(&packet, USB_TIMEOUT)?;
                    anyhow::ensure!(written == packet.len(), "short picture packet write");
                    std::thread::sleep(Duration::from_millis(2));
                }
                Ok(())
            },
        )?;
        Ok((next_seq, sent_at))
    }

    fn check_picture_transfer(
        &self,
        cancel: &AtomicBool,
        deadline: std::time::Instant,
    ) -> Result<()> {
        ensure!(
            !cancel.load(Ordering::Acquire) && !self.poll_stop.load(Ordering::Acquire),
            "Wireless image upload cancelled"
        );
        ensure!(
            std::time::Instant::now() < deadline,
            "Wireless image upload timed out"
        );
        Ok(())
    }

    fn wait_pic_ack(
        &self,
        mac: &[u8; 6],
        expected: (u8, std::time::Instant),
        cancel: &AtomicBool,
        transfer_deadline: std::time::Instant,
    ) -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            self.check_picture_transfer(cancel, transfer_deadline)?;
            if self.wireless_theme_acked(mac, expected.0, expected.1) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bail!("picture command acknowledgement timed out for {mac:02x?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h2_picture_upload_rejects_wrong_models_and_firmware_buffer_overflow() {
        for model in [10, 11] {
            assert!(validate_aio_image(model, 1).is_ok());
            assert!(validate_aio_image(model, 20_480).is_ok());
            for size in [0, 20_481, 40_960, 55_880] {
                assert!(validate_aio_image(model, size).is_err());
            }
        }
        for model in [0, 23, 27, 88] {
            assert!(validate_aio_image(model, 690).is_err());
        }
    }

    #[test]
    fn picture_packets_route_to_receiver_and_zero_pad_final_chunk() {
        let mut record = [0; 42];
        record[..6].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        record[12] = 12;
        record[13] = 2;
        record[18] = 10;
        record[41] = 0x1c;
        let device = super::super::discovery::parse_device_record(&record, 0).unwrap();
        let packets = picture_packets(&device, &[9; 6], 8, 7, 93, &[0xab; 20]).unwrap();
        let mut rf = Vec::new();
        for (index, packet) in packets.iter().enumerate() {
            assert_eq!(&packet[..4], &[0x10, index as u8, 12, 2]);
            rf.extend_from_slice(&packet[4..]);
        }
        assert_eq!(
            &rf[..19],
            &[0x12, 0x22, 1, 2, 3, 4, 5, 6, 9, 9, 9, 9, 9, 9, 2, 8, 0, 7, 93]
        );
        assert_eq!(&rf[19..39], &[0xab; 20]);
        assert!(rf[39..].iter().all(|&b| b == 0));
        let terminal =
            picture_packets(&device, &[9; 6], 8, 8, 255, &20_480u16.to_be_bytes()).unwrap();
        assert_eq!(&terminal[0][22..25], &[255, 0x50, 0]);
        assert!(picture_packets(&device, &[9; 6], 8, 8, 1, &[0; 221]).is_err());
    }

    #[test]
    fn cancelled_or_expired_upload_stops_before_more_commands() {
        let controller = WirelessController::new();
        assert!(controller
            .check_picture_transfer(
                &AtomicBool::new(true),
                std::time::Instant::now() + Duration::from_secs(1)
            )
            .is_err());
        assert!(controller
            .check_picture_transfer(&AtomicBool::new(false), std::time::Instant::now())
            .is_err());
    }

    #[test]
    fn image_reservation_blocks_sequence_commands_but_not_cooling_or_other_devices() {
        let controller = WirelessController::new();
        let mut record = [0; 42];
        record[..6].copy_from_slice(&[1; 6]);
        record[12] = 8;
        record[18] = 10;
        record[41] = 0x1c;
        let mut device = super::super::discovery::parse_device_record(&record, 0).unwrap();
        *controller.picture_target.lock() = Some(device.mac);
        let enqueue = |device: &DiscoveredDevice, ack| {
            controller
                .enqueue_rf_command(device, vec![0; RF_DATA_SIZE], ack, "test")
                .unwrap_err()
                .to_string()
        };
        assert!(enqueue(&device, AckSignal::CmdSeq(1)).contains("image upload"));
        assert!(enqueue(&device, AckSignal::Pwm([255; 4])).contains("TX is unavailable"));
        device.mac = [2; 6];
        assert!(enqueue(&device, AckSignal::CmdSeq(1)).contains("TX is unavailable"));
    }
}
