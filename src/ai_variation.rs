use std::str::FromStr;
use crate::{arc_vec, ChooserTable, Melody, MelodyMaker, PendingNote, SliderValue, SynthChoice, FromAiMsg, send_recorded_melody, analyzer};
use crossbeam_queue::SegQueue;
use midi_msg::{ChannelVoiceMsg, MidiMsg};
use std::sync::{Arc, Mutex};
use crossbeam_utils::atomic::AtomicCell;
use eframe::emath::Numeric;

pub type AIFuncType = dyn Fn(&mut MelodyMaker, &Melody, f64) -> Melody + Send + Sync;
pub type AITable = ChooserTable<Arc<AIFuncType>>;

pub fn make_ai_table() -> AITable {
    let ai_funcs: Vec<(&str, Arc<AIFuncType>)> = arc_vec![
        ("Bypass", |_, _, _| Melody::new()),
        ("Emphasis-Anchored Choice", MelodyMaker::create_emphasis_variation),
        ("Consistent Figure Replacement", MelodyMaker::create_figure_mapped_variation),
        ("Whimsical Variation", MelodyMaker::create_whimsical_variation)
    ];
    ChooserTable::from(&ai_funcs)
}

pub fn start_ai_thread(
    ai_table: Arc<Mutex<AITable>>,
    input2ai: Arc<SegQueue<MidiMsg>>,
    ai2output: Arc<SegQueue<(SynthChoice, MidiMsg)>>,
    ai2dbase: Arc<SegQueue<FromAiMsg>>,
    replay_delay_slider: Arc<AtomicCell<SliderValue<f64>>>,
    ornament_gap_slider: Arc<AtomicCell<SliderValue<i64>>>,
    p_random_slider: Arc<AtomicCell<SliderValue<f64>>>,
    p_ornament_slider: Arc<AtomicCell<SliderValue<f64>>>,
) {
    std::thread::spawn(move || {
        let mut recorder = PlayerRecorder::new(
            input2ai,
            ai2output.clone(),
            replay_delay_slider.clone(),
        );
        let mut performer =
            Performer::new(p_random_slider, p_ornament_slider,ornament_gap_slider, ai_table);
        let min_melody_pitches = *analyzer::FIGURE_LENGTHS.iter().min().unwrap();

        loop {
            let melody = recorder.record();
            let variation = performer.create_variation(&melody);
            if variation.num_pitch_changes() >= min_melody_pitches && variation.duration() > replay_delay_slider.load().current() {
                ai2dbase.push(FromAiMsg {melody, variation: variation.clone()});
                send_recorded_melody(&variation, SynthChoice::Ai, ai2output.clone());
            }
        }
    });
}

struct PlayerRecorder {
    input2ai: Arc<SegQueue<MidiMsg>>,
    ai2output: Arc<SegQueue<(SynthChoice, MidiMsg)>>,
    replay_delay_slider: Arc<AtomicCell<SliderValue<f64>>>,
    waiting: Option<PendingNote>,
    player_melody: Melody,
}

impl PlayerRecorder {
    fn new(
        input2ai: Arc<SegQueue<MidiMsg>>,
        ai2output: Arc<SegQueue<(SynthChoice, MidiMsg)>>,
        replay_delay_slider: Arc<AtomicCell<SliderValue<f64>>>,
    ) -> Self {
        PlayerRecorder {
            input2ai,
            ai2output,
            replay_delay_slider,
            waiting: None,
            player_melody: Melody::new(),
        }
    }

    fn record(&mut self) -> Melody {
        self.waiting = None;
        let mut player_finished = false;
        while !player_finished {
            if let Some(msg) = self.input2ai.pop() {
                self.handle_incoming(msg);
            }

            if let Some(pending_note) = self.waiting {
                player_finished = self.check_if_finished(pending_note);
            }
        }
        let mut result = Melody::new();
        std::mem::swap(&mut result, &mut self.player_melody);
        result.synchronize_rests();
        result
    }

    fn handle_incoming(&mut self, msg: MidiMsg) {
        if let MidiMsg::ChannelVoice { channel: _, msg } = msg {
            match msg {
                ChannelVoiceMsg::NoteOff { note, velocity }
                | ChannelVoiceMsg::NoteOn { note, velocity } => {
                    if let Some(pending_note) = self.waiting {
                        self.player_melody.add(pending_note.into());
                    }
                    self.waiting = Some(PendingNote::new(note, velocity));
                }
                _ => {}
            }
        }
        self.ai2output.push((SynthChoice::Human, msg));
    }

    fn check_if_finished(&mut self, pending_note: PendingNote) -> bool {
        let replay_delay = self.replay_delay_slider.load();
        if pending_note.is_rest() && pending_note.elapsed() > replay_delay.current() {
            self.player_melody.add(pending_note.into());
            true
        } else {
            false
        }
    }
}

struct Performer {
    maker: MelodyMaker,
    p_random_slider: Arc<AtomicCell<SliderValue<f64>>>,
    p_ornament_slider: Arc<AtomicCell<SliderValue<f64>>>,
    ornament_gap_slider: Arc<AtomicCell<SliderValue<i64>>>,
    ai_table: Arc<Mutex<AITable>>
}

impl Performer {
    fn new(
        p_random_slider: Arc<AtomicCell<SliderValue<f64>>>,
        p_ornament_slider: Arc<AtomicCell<SliderValue<f64>>>,
        ornament_gap_slider: Arc<AtomicCell<SliderValue<i64>>>,
        ai_table: Arc<Mutex<AITable>>
    ) -> Self {
        Performer {
            maker: MelodyMaker::new(),
            p_random_slider,
            p_ornament_slider,
            ornament_gap_slider,
            ai_table
        }
    }

    fn from_slider<N:FromStr + Numeric>(slider: &Arc<AtomicCell<SliderValue<N>>>) -> N {
        slider.load().current()
    }

    fn create_variation(&mut self, melody: &Melody) -> Melody {
        let p_random = Self::from_slider(&self.p_random_slider);
        let p_ornament = Self::from_slider(&self.p_ornament_slider);
        let ornament_gap = Self::from_slider(&self.ornament_gap_slider);
        let var_func = {
            let ai_table = self.ai_table.lock().unwrap();
            ai_table.current_choice()
        };
        let variation = var_func(&mut self.maker, &melody, p_random);
        self.maker.ornamented(&variation, p_ornament, ornament_gap)
    }
}
