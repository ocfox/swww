use image::gif::GifDecoder;
use image::io::Reader;
use image::{self, imageops::FilterType, GenericImageView};
use image::{AnimationDecoder, ImageFormat};
use log::{debug, info};

use smithay_client_toolkit::reexports::calloop::channel::Sender;
use smithay_client_toolkit::reexports::calloop::channel::{self, Channel};

use std::io::BufReader;
use std::time::{Duration, Instant};
use std::{path::Path, sync::mpsc, thread};

pub enum ProcessingResult {
    Img((Vec<String>, Vec<u8>)),
    Gif((Channel<(Vec<String>, Vec<u8>)>, mpsc::Sender<Vec<String>>)),
}

use miniz_oxide::deflate;

///Waits for a msg from the daemon event_loop
pub fn processor_loop(msg: (Vec<String>, (u32, u32), FilterType, &Path)) -> ProcessingResult {
    let answer = handle_msg(msg.0, msg.1, msg.2, msg.3);
    debug!("Finished image processing!");
    answer
}

fn handle_msg(
    outputs: Vec<String>,
    dimensions: (u32, u32),
    filter: FilterType,
    path: &Path,
) -> ProcessingResult {
    let (width, height) = dimensions;

    //We check if we can open and read the image before sending it, so these should never fail
    let img_buf = image::io::Reader::open(&path)
        .expect("Failed to open image, though this should be impossible...");
    match img_buf.format() {
        Some(ImageFormat::Gif) => process_gif(img_buf, width, height, outputs, filter),

        None => unreachable!("Unsupported format. This also should be impossible..."),
        _ => {
            let img = img_buf
                .decode()
                .expect("Img decoding failed, though this should be impossible...");
            let img_bytes = img_resize(img, width, height, filter);
            ProcessingResult::Img((outputs, img_bytes))
        }
    }
}

fn process_gif(
    gif_buf: Reader<BufReader<std::fs::File>>,
    width: u32,
    height: u32,
    outputs: Vec<String>,
    filter: FilterType,
) -> ProcessingResult {
    let (sender, receiver) = channel::channel();
    let (stop_sender, stop_receiver) = mpsc::channel();
    thread::spawn(move || {
        animate(
            gif_buf,
            outputs,
            width,
            height,
            filter,
            sender,
            stop_receiver,
        )
    });
    ProcessingResult::Gif((receiver, stop_sender))
}

fn animate(
    gif: Reader<BufReader<std::fs::File>>,
    mut outputs: Vec<String>,
    width: u32,
    height: u32,
    filter: FilterType,
    sender: Sender<(Vec<String>, Vec<u8>)>,
    receiver: mpsc::Receiver<Vec<String>>,
) {
    let mut frames = GifDecoder::new(gif.into_inner())
        .expect("Couldn't decode gif, though this should be impossible...")
        .into_frames();

    let (frame_sender, frame_recv) = mpsc::channel();
    let (cache_sender, cache_recv) = mpsc::channel();
    thread::spawn(move || cache_the_frames(frame_recv, cache_sender));

    //first loop
    let mut now = Instant::now();
    while let Some(frame) = frames.next() {
        let frame = frame.unwrap();
        let (dur_num, dur_div) = frame.delay().numer_denom_ms();
        let duration = Duration::from_millis((dur_num / dur_div).into());
        let img = img_resize(
            image::DynamicImage::ImageRgba8(frame.into_buffer()),
            width,
            height,
            filter,
        );

        frame_sender
            .send((img.clone(), duration))
            .unwrap_or_else(|_| return);

        match receiver.recv_timeout(duration.saturating_sub(now.elapsed())) {
            Ok(out_to_remove) => {
                outputs.retain(|o| !out_to_remove.contains(o));
                if outputs.is_empty() {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                debug!("Receiver disconnected! Stopping animation...");
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => (),
        };
        sender
            .send((outputs.clone(), img))
            .unwrap_or_else(|_| return);
        now = Instant::now();
    }
    drop(frame_sender); //This will make the loop in the other thread exit
    let cached_frames = cache_recv.recv().unwrap();

    //If there was only one frame, we leave immediatelly, since no animation is necessary
    if cached_frames.len() == 1 {
        return;
    }

    loop_animation(&cached_frames, outputs, sender, receiver);
}

fn cache_the_frames(
    frame_recv: mpsc::Receiver<(Vec<u8>, Duration)>,
    cache_sender: mpsc::Sender<Vec<(Vec<u8>, Duration)>>,
) {
    let mut cached_frames = Vec::new();
    while let Ok((uncached, duration)) = frame_recv.recv() {
        cached_frames.push((deflate::compress_to_vec(&uncached, 6), duration));
    }
    cache_sender.send(cached_frames).unwrap_or_else(|_| return);
}

fn loop_animation(
    cached_frames: &[(Vec<u8>, Duration)],
    mut outputs: Vec<String>,
    sender: Sender<(Vec<String>, Vec<u8>)>,
    receiver: mpsc::Receiver<Vec<String>>,
) {
    info!("Finished caching the frames!");
    let mut now = Instant::now();
    loop {
        for (cached_img, duration) in cached_frames {
            let img = miniz_oxide::inflate::decompress_to_vec(&cached_img).unwrap();
            match receiver.recv_timeout(duration.saturating_sub(now.elapsed())) {
                Ok(out_to_remove) => {
                    outputs.retain(|o| !out_to_remove.contains(o));
                    if outputs.is_empty() {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => (),
            };
            sender
                .send((outputs.clone(), img))
                .unwrap_or_else(|_| return);
            now = Instant::now();
        }
    }
}

fn img_resize(img: image::DynamicImage, width: u32, height: u32, filter: FilterType) -> Vec<u8> {
    let img_dimensions = img.dimensions();
    debug!("Output dimensions: width: {} height: {}", width, height);
    debug!(
        "Image dimensions:  width: {} height: {}",
        img_dimensions.0, img_dimensions.1
    );
    let resized_img = if img_dimensions != (width, height) {
        info!("Image dimensions are different from output's. Resizing...");
        img.resize_to_fill(width, height, filter)
    } else {
        info!("Image dimensions are identical to output's. Skipped resize!!");
        img
    };

    // The ARGB is 'little endian', so here we must  put the order
    // of bytes 'in reverse', so it needs to be BGRA.
    resized_img.into_bgra8().into_raw()
}
