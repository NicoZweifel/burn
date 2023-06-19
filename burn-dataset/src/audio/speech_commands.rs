use crate::{
    transform::{Mapper, MapperDataset},
    Dataset, HuggingfaceDatasetLoader, SqliteDataset,
};

use hound::WavReader;
use serde::{Deserialize, Serialize};
use strum_macros::{Display, FromRepr};

type MappedDataset = MapperDataset<SqliteDataset<SpeechItemRaw>, ConvertSamples, SpeechItemRaw>;

/// Enum representing speech command classes in the Speech Commands dataset.
/// Class names are based on the Speech Commands dataset from Huggingface.
/// See: https://huggingface.co/datasets/speech_commands
#[derive(Debug, Display, Clone, Copy, FromRepr, Serialize, Deserialize)]
pub enum SpeechCommandClass {
    // Target command words
    Yes = 0,
    No = 1,
    Up = 2,
    Down = 3,
    Left = 4,
    Right = 5,
    On = 6,
    Off = 7,
    Stop = 8,
    Go = 9,
    Zero = 10,
    One = 11,
    Two = 12,
    Three = 13,
    Four = 14,
    Five = 15,
    Six = 16,
    Seven = 17,
    Eight = 18,
    Nine = 19,

    // Non-target words grouped into "Other"
    Bed = 20,
    Bird = 21,
    Cat = 22,
    Dog = 23,
    Happy = 24,
    House = 25,
    Marvin = 26,
    Sheila = 27,
    Tree = 28,
    Wow = 29,

    // Commands from v2 dataset, grouped into "Other"
    Backward = 30,
    Forward = 31,
    Follow = 32,
    Learn = 33,
    Visual = 34,

    // Background noise
    Silence = 35,

    // Other miscellaneous words
    Other = 36,
}

/// Struct containing raw speech data returned from a database.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpeechItemRaw {
    pub audio_bytes: Vec<u8>,
    pub label: usize,
    pub is_unknown: bool,
}

/// Speech item with audio samples and label.
///
/// The audio samples are floats in the range [-1.0, 1.0].
/// The sample rate is in Hz.
/// The label is the class index (see [SpeechCommandClass]).
/// To convert to usize simply use `as usize`. To convert label to string use `.to_string()`.
///
/// The original label is also stored in the `label_original` field for debugging and remapping if needed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpeechItem {
    /// Audio samples in the range [-1.0, 1.0].
    pub audio_samples: Vec<f32>,

    /// The sample rate of the audio.
    pub sample_rate: usize,

    /// 20 target words, silence and other
    pub label: SpeechCommandClass,

    /// The original label for debugging and remapping if needed.
    pub label_original: SpeechCommandClass,
}

/// Speech Commands dataset from Huggingface v0.02.
/// See [Speech Commands dataset](https://huggingface.co/datasets/speech_commands).
///
/// The data is downloaded from Huggingface and stored in a SQLite database (3.0 GB).
/// The dataset contains 99,720 audio samples of 2,607 people saying 35 different words.
///
/// The labels are 20 target words, silence and other (22 classes).
///
/// Note: label class indices are not continuous because some classes are grouped together.
/// This may create gaps in hot encoded labels (effectively wasting some memory).
///
/// The dataset is split into 3 parts:
/// - train: 84,848 audio files
/// - test: 4,890 audio files
/// - validation: 9,982 audio files
pub struct SpeechCommandsDataset {
    dataset: MappedDataset,
}

impl SpeechCommandsDataset {
    /// Create a new dataset with the given split.
    pub fn new(split: &str) -> Self {
        let dataset: SqliteDataset<SpeechItemRaw> =
            HuggingfaceDatasetLoader::new("speech_commands")
                .with_subset("v0.02")
                .dataset(split)
                .unwrap();
        let dataset = MapperDataset::new(dataset, ConvertSamples);
        Self { dataset }
    }

    /// Create a new dataset with the train split.
    pub fn train() -> Self {
        Self::new("train")
    }

    /// Create a new dataset with the test split.
    pub fn test() -> Self {
        Self::new("test")
    }

    /// Create a new dataset with the validation split.
    pub fn validation() -> Self {
        Self::new("validation")
    }

    /// Returns the number of classes in the dataset
    pub fn num_classes() -> usize {
        22 // 10 command words + 10 digits + 1 silence + 1 other
    }
}

impl Dataset<SpeechItem> for SpeechCommandsDataset {
    fn get(&self, index: usize) -> Option<SpeechItem> {
        self.dataset.get(index)
    }

    fn len(&self) -> usize {
        self.dataset.len()
    }
}

/// Mapper converting audio bytes into audio samples and the label to enum class.
struct ConvertSamples;

impl ConvertSamples {
    /// Convert label to enum class and select the target classes.
    /// See the original paper (section 5.2) https://arxiv.org/pdf/1804.03209.pdf
    fn word_choice(label: usize) -> SpeechCommandClass {
        match label {
            0..=19 => Self::to_speechcommandclass(label),
            35 => SpeechCommandClass::Silence,
            _ => SpeechCommandClass::Other,
        }
    }

    /// Convert label to enum class.
    fn to_speechcommandclass(label: usize) -> SpeechCommandClass {
        SpeechCommandClass::from_repr(label).unwrap()
    }

    /// Convert audio bytes into samples of floats [-1.0, 1.0].
    fn to_audiosamples(bytes: &Vec<u8>) -> (Vec<f32>, usize) {
        let reader = WavReader::new(bytes.as_slice()).unwrap();
        let spec = reader.spec();

        // Maximum value of the audio samples (using bit shift to raise 2 to the power of bits per sample).
        let max_value = (1 << (spec.bits_per_sample - 1)) as f32;

        // The sample rate of the audio.
        let sample_rate = spec.sample_rate as usize;

        // Convert the audio samples to floats [-1.0, 1.0].
        let audio_samples: Vec<f32> = reader
            .into_samples::<i32>()
            .filter_map(Result::ok)
            .map(|sample| sample as f32 / max_value)
            .collect();

        (audio_samples, sample_rate)
    }
}

impl Mapper<SpeechItemRaw, SpeechItem> for ConvertSamples {
    /// Convert audio bytes into samples of floats [-1.0, 1.0]
    /// and the label to enum class with the target word, other and silence classes.
    ///
    /// Note: The orginal label is also stored in the `label_original` field for debugging
    /// and remapping if needed.
    fn map(&self, item: &SpeechItemRaw) -> SpeechItem {
        let (audio_samples, sample_rate) = Self::to_audiosamples(&item.audio_bytes);

        // Convert the label to enum class, with the target words, other and silence classes.
        let label = Self::word_choice(item.label);

        // Keep the original label for debugging and remapping if needed.
        let label_original = Self::to_speechcommandclass(item.label);

        SpeechItem {
            audio_samples,
            sample_rate,
            label,
            label_original,
        }
    }
}