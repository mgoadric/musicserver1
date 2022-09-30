use std::sync::Arc;
use anyhow::bail;
use midir::{MidiInput, Ignore, MidiInputPort};
use musicserver1::{input_cmd, usize_input};
use midi_msg::{ChannelVoiceMsg, MidiMsg};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use fundsp::hacker::*;
use crossbeam_queue::SegQueue;
use dashmap::DashSet;
use enum_iterator::{all, Sequence};

fn main() -> anyhow::Result<()> {
    let mut midi_in = MidiInput::new("midir reading input")?;
    let in_port = get_midi_device(&mut midi_in)?;

    let midi_queue = Arc::new(SegQueue::new());
    start_output(midi_queue.clone())?;
    start_input(midi_queue, midi_in, in_port)
}

fn get_midi_device(midi_in: &mut MidiInput) -> anyhow::Result<MidiInputPort> {
    midi_in.ignore(Ignore::None);

    let in_ports = midi_in.ports();
    match in_ports.len() {
        0 => bail!("no input port found"),
        1 => {
            println!("Choosing the only available input port: {}", midi_in.port_name(&in_ports[0]).unwrap());
            Ok(in_ports[0].clone())
        },
        _ => {
            println!("\nAvailable input ports:");
            for (i, p) in in_ports.iter().enumerate() {
                println!("{}: {}", i, midi_in.port_name(p).unwrap());
            }
            let input = input_cmd("Please select input port: ")?;
            match in_ports.get(input.trim().parse::<usize>()?) {
                None => bail!("invalid input port selected"),
                Some(p) => Ok(p.clone())
            }
        }
    }
}

fn start_output(midi_queue: Arc<SegQueue<MidiMsg>>) -> anyhow::Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .expect("failed to find a default output device");
    let config = device.default_output_config().unwrap();

    let synth = SynthSound::pick_synth()?;

    match config.sample_format() {
        cpal::SampleFormat::F32 => run::<f32>(midi_queue.clone(), device, config.into(), synth).unwrap(),
        cpal::SampleFormat::I16 => run::<i16>(midi_queue.clone(), device, config.into(), synth).unwrap(),
        cpal::SampleFormat::U16 => run::<u16>(midi_queue.clone(), device, config.into(), synth).unwrap(),
    }
    Ok(())
}

fn start_input(midi_queue: Arc<SegQueue<MidiMsg>>, mut midi_in: MidiInput, in_port: MidiInputPort) -> anyhow::Result<()> {
    println!("\nOpening connection");
    let in_port_name = midi_in.port_name(&in_port)?;

    // _conn_in needs to be a named parameter, because it needs to be kept alive until the end of the scope
    let _conn_in = midi_in.connect(&in_port, "midir-read-input", move |_stamp, message, _| {
        let (msg, _len) = MidiMsg::from_midi(&message).unwrap();
        midi_queue.push(msg);
    }, ()).unwrap();

    println!("Connection open, reading input from '{in_port_name}'");

    let _ = input_cmd("(press enter to exit)...\n")?;
    println!("Closing connection");
    Ok(())
}

#[derive(Copy,Clone,Sequence,Debug)]
enum SynthSound {
    SinPulse, SimpleTri
}

impl SynthSound {
    fn sound(&self, note: u8, velocity: u8) -> Box<dyn AudioUnit64> {
        match self {
            SynthSound::SinPulse => {
                Box::new(lfo(move |t| {
                    (midi_hz(note as f64), lerp11(0.01, 0.99, sin_hz(0.05, t)))
                }) >> pulse() * (velocity as f64 / 127.0))
            }
            SynthSound::SimpleTri => {
                Box::new(lfo(move |_t| {
                    midi_hz(note as f64)
                }) >> triangle() * (velocity as f64 / 127.0))
            }
        }
    }

    fn pick_synth() -> std::io::Result<Self> {
        let synths: Vec<Self> = all::<Self>().collect();
        for (i, s) in synths.iter().enumerate() {
            println!("{}) {s:?}", i+1);
        }
        let choice = usize_input("Enter choice:", 1..=synths.len())?;
        Ok(synths[choice - 1])
    }
}

fn run<T>(incoming: Arc<SegQueue<MidiMsg>>, device: cpal::Device, config: cpal::StreamConfig, synth: SynthSound) -> anyhow::Result<()>
    where
        T: cpal::Sample,
{
    let run_inst = RunInstance {
        synth,
        sample_rate: config.sample_rate.0 as f64,
        channels: config.channels as usize,
        incoming: incoming.clone(),
        device: Arc::new(device),
        config: Arc::new(config),
        notes_in_use: Arc::new(DashSet::new())
    };

    std::thread::spawn(move || {
        run_inst.listen_play_loop::<T>();
    });

    Ok(())
}

#[derive(Clone)]
struct RunInstance {
    synth: SynthSound,
    sample_rate: f64,
    channels: usize,
    incoming: Arc<SegQueue<MidiMsg>>,
    device: Arc<cpal::Device>,
    config: Arc<cpal::StreamConfig>,
    notes_in_use: Arc<DashSet<u8>>
}

impl RunInstance {
    fn listen_play_loop<T: cpal::Sample>(&self) {
        loop {
            if let Some(m) = self.incoming.pop() {
                if let MidiMsg::ChannelVoice { channel:_, msg} = m {
                    println!("{msg:?}");
                    match msg {
                        ChannelVoiceMsg::NoteOff {note, velocity:_} => {
                            self.notes_in_use.remove(&note);
                        }
                        ChannelVoiceMsg::NoteOn {note, velocity} => {
                            self.notes_in_use.insert(note);
                            let mut c = self.synth.sound(note, velocity);
                            c.reset(Some(self.sample_rate));
                            println!("{:?}", c.get_stereo());
                            self.play_sound::<T>(note, c);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn play_sound<T: cpal::Sample>(&self, note: u8, mut sound: Box<dyn AudioUnit64>) {
        let mut next_value = move || sound.get_stereo();
        let notes_in_use = self.notes_in_use.clone();
        let device = self.device.clone();
        let config = self.config.clone();
        let channels = self.channels;
        std::thread::spawn(move || {
            let err_fn = |err| eprintln!("an error occurred on stream: {err}");
            let stream = device.build_output_stream(
                &config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    write_data(data, channels, &mut next_value)
                },
                err_fn,
            ).unwrap();

            stream.play().unwrap();
            while notes_in_use.contains(&note) {}
        });
    }
}

fn write_data<T>(output: &mut [T], channels: usize, next_sample: &mut dyn FnMut() -> (f64, f64))
    where
        T: cpal::Sample,
{
    for frame in output.chunks_mut(channels) {
        let sample = next_sample();
        let left: T = cpal::Sample::from::<f32>(&(sample.0 as f32));
        let right: T = cpal::Sample::from::<f32>(&(sample.1 as f32));

        for (channel, sample) in frame.iter_mut().enumerate() {
            if channel & 1 == 0 {
                *sample = left;
            } else {
                *sample = right;
            }
        }
    }
}