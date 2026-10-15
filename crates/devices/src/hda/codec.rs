//! The codec behind the controller: an output converter (DAC) feeding a
//! speaker pin and, when the VM may use the microphone, an input converter
//! (ADC) fed by a mic pin. Like QEMU's hda-duplex, with fixed (built-in)
//! pins so the guest needs no jack detection.
//!
//! Verbs come from guest memory (CORB) or a register: NIDs, indices and
//! gains are range-checked; unknown verbs and nodes answer 0.

use serde::{Deserialize, Serialize};

pub const NID_AFG: u8 = 1;
pub const NID_DAC: u8 = 2;
pub const NID_OUT: u8 = 3;
pub const NID_ADC: u8 = 4;
pub const NID_MIC: u8 = 5;

// Parameters (verb F00).
const P_VENDOR: u8 = 0x00;
const P_REVISION: u8 = 0x02;
const P_NODES: u8 = 0x04;
const P_FG_TYPE: u8 = 0x05;
const P_AFG_CAPS: u8 = 0x08;
const P_WIDGET_CAPS: u8 = 0x09;
const P_PCM: u8 = 0x0a;
const P_FORMATS: u8 = 0x0b;
const P_PIN_CAPS: u8 = 0x0c;
const P_IN_AMP: u8 = 0x0d;
const P_CONN_LEN: u8 = 0x0e;
const P_POWER: u8 = 0x0f;
const P_OUT_AMP: u8 = 0x12;

/// 0 dB is the top step; each step is 1 dB.
pub const AMP_STEPS: u8 = 0x4a;
const AMP_CAPS: u32 = 1 << 31 | 3 << 16 | (AMP_STEPS as u32) << 8 | AMP_STEPS as u32;
/// 16-bit samples at 44.1 and 48 kHz.
const PCM_CAPS: u32 = 1 << 17 | 1 << 6 | 1 << 5;
// Widget capabilities: type, stereo, amps, format override, connection list.
const WCAP_DAC: u32 = 0x1d;
const WCAP_ADC: u32 = 0x10_011b;
const WCAP_OUT_PIN: u32 = 0x40_0101;
const WCAP_MIC_PIN: u32 = 0x40_0001;
const PINCAP_OUT: u32 = 1 << 4;
const PINCAP_IN: u32 = 1 << 5;
/// Config defaults: fixed internal speaker, fixed internal mic.
const CFG_OUT_PIN: u32 = 0x9017_0110;
const CFG_MIC_PIN: u32 = 0x90a6_0120;

/// Names the guest reads (from the VM's `[identity]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecIds {
    pub vendor: u16,
    pub device: u16,
    pub subsys: u32,
}

impl Default for CodecIds {
    /// QEMU's hda-duplex.
    fn default() -> Self {
        CodecIds { vendor: 0x1af4, device: 0x0022, subsys: 0x1af4_0022 }
    }
}

/// What the guest set up (part of snapshots).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecState {
    /// Converter stream tag << 4 | channel.
    pub dac_stream: u8,
    pub adc_stream: u8,
    pub dac_fmt: u16,
    pub adc_fmt: u16,
    /// Left, right: mute bit 7, gain 0..=AMP_STEPS.
    pub dac_amp: [u8; 2],
    pub adc_amp: [u8; 2],
    pub pin_ctl: [u8; 2],
    pub eapd: u8,
    pub power: [u8; 6],
    pub unsol: [u8; 6],
}

impl Default for CodecState {
    fn default() -> Self {
        CodecState {
            dac_stream: 0,
            adc_stream: 0,
            dac_fmt: 0x11,
            adc_fmt: 0x11,
            dac_amp: [AMP_STEPS; 2],
            adc_amp: [AMP_STEPS; 2],
            pin_ctl: [0x40, 0x20],
            eapd: 0x02,
            power: [0; 6],
            unsol: [0; 6],
        }
    }
}

#[derive(Debug, Clone)]
pub struct Codec {
    ids: CodecIds,
    mic: bool,
    pub st: CodecState,
}

impl Codec {
    pub fn new(ids: CodecIds, mic: bool) -> Self {
        Codec { ids, mic, st: CodecState::default() }
    }

    pub fn has_mic(&self) -> bool {
        self.mic
    }

    fn exists(&self, nid: u8) -> bool {
        nid <= NID_OUT || (self.mic && nid <= NID_MIC)
    }

    /// Stream tag the DAC listens to (0: none).
    pub fn dac_tag(&self) -> u8 {
        self.st.dac_stream >> 4
    }

    pub fn adc_tag(&self) -> u8 {
        if self.mic {
            self.st.adc_stream >> 4
        } else {
            0
        }
    }

    fn param(&self, nid: u8, p: u8) -> u32 {
        let last = if self.mic { NID_MIC } else { NID_OUT };
        match (nid, p) {
            (0, P_VENDOR) => (self.ids.vendor as u32) << 16 | self.ids.device as u32,
            (0, P_REVISION) => 0x0010_0101,
            (0, P_NODES) => (NID_AFG as u32) << 16 | 1,
            (NID_AFG, P_NODES) => (NID_DAC as u32) << 16 | (last - NID_DAC + 1) as u32,
            (NID_AFG, P_FG_TYPE) => 0x01,
            (NID_AFG, P_AFG_CAPS) => 0x0808,
            (NID_AFG | NID_DAC | NID_ADC, P_PCM) => PCM_CAPS,
            (NID_AFG | NID_DAC | NID_ADC, P_FORMATS) => 1,
            (NID_AFG, P_POWER) => 0x0f,
            (NID_AFG | NID_DAC, P_OUT_AMP) | (NID_AFG | NID_ADC, P_IN_AMP) => AMP_CAPS,
            (NID_DAC, P_WIDGET_CAPS) => WCAP_DAC,
            (NID_OUT, P_WIDGET_CAPS) => WCAP_OUT_PIN,
            (NID_ADC, P_WIDGET_CAPS) => WCAP_ADC,
            (NID_MIC, P_WIDGET_CAPS) => WCAP_MIC_PIN,
            (NID_OUT, P_PIN_CAPS) => PINCAP_OUT,
            (NID_MIC, P_PIN_CAPS) => PINCAP_IN,
            (NID_OUT | NID_ADC, P_CONN_LEN) => 1,
            _ => 0,
        }
    }

    fn amp(&self, nid: u8) -> Option<[u8; 2]> {
        match nid {
            NID_DAC => Some(self.st.dac_amp),
            NID_ADC => Some(self.st.adc_amp),
            _ => None,
        }
    }

    /// Run one verb. None: no codec at that address (no response at all,
    /// like real hardware).
    pub fn verb(&mut self, cmd: u32) -> Option<u32> {
        if cmd >> 28 != 0 {
            return None;
        }
        let nid = ((cmd >> 20) & 0xff) as u8;
        if !self.exists(nid) {
            return Some(0);
        }
        let v4 = (cmd >> 16) & 0xf;
        if v4 != 0x7 && v4 != 0xf {
            return Some(self.verb4(nid, v4, (cmd & 0xffff) as u16));
        }
        let (verb, arg) = ((cmd >> 8) & 0xfff, (cmd & 0xff) as u8);
        let i = nid as usize;
        let pin = match nid {
            NID_OUT => Some(0),
            NID_MIC => Some(1),
            _ => None,
        };
        Some(match verb {
            0xf00 => self.param(nid, arg),
            // Connection list: the one entry, short form.
            0xf02 if arg == 0 => match nid {
                NID_OUT => NID_DAC as u32,
                NID_ADC => NID_MIC as u32,
                _ => 0,
            },
            0xf05 => {
                let p = self.st.power[i] as u32 & 3;
                p << 4 | p
            }
            0x705 => {
                self.st.power[i] = arg & 3;
                0
            }
            0xf06 => match nid {
                NID_DAC => self.st.dac_stream as u32,
                NID_ADC => self.st.adc_stream as u32,
                _ => 0,
            },
            0x706 => {
                match nid {
                    NID_DAC => self.st.dac_stream = arg,
                    NID_ADC => self.st.adc_stream = arg,
                    _ => {}
                }
                0
            }
            0xf07 => pin.map_or(0, |p| self.st.pin_ctl[p] as u32),
            0x707 => {
                if let Some(p) = pin {
                    self.st.pin_ctl[p] = arg;
                }
                0
            }
            0xf08 => self.st.unsol[i] as u32,
            0x708 => {
                self.st.unsol[i] = arg & 0xbf;
                0
            }
            // Pin sense: built in, always there.
            0xf09 if pin.is_some() => 1 << 31,
            0xf0c if nid == NID_OUT => self.st.eapd as u32,
            0x70c if nid == NID_OUT => {
                self.st.eapd = arg & 7;
                0
            }
            0xf1c => match nid {
                NID_OUT => CFG_OUT_PIN,
                NID_MIC => CFG_MIC_PIN,
                _ => 0,
            },
            0xf20 if nid == NID_AFG => self.ids.subsys,
            // Converter channel count: stereo.
            0xf2d if matches!(nid, NID_DAC | NID_ADC) => 1,
            // Function group reset.
            0x7ff if nid == NID_AFG => {
                self.st = CodecState::default();
                0
            }
            _ => 0,
        })
    }

    fn verb4(&mut self, nid: u8, v: u32, arg: u16) -> u32 {
        match v {
            // Set / get converter format.
            0x2 => {
                match nid {
                    NID_DAC => self.st.dac_fmt = arg,
                    NID_ADC => self.st.adc_fmt = arg,
                    _ => {}
                }
                0
            }
            0xa => match nid {
                NID_DAC => self.st.dac_fmt as u32,
                NID_ADC => self.st.adc_fmt as u32,
                _ => 0,
            },
            // Set amp: output/input, left/right, mute, gain.
            0x3 => {
                let ok = match nid {
                    NID_DAC => arg & 0x8000 != 0,
                    NID_ADC => arg & 0x4000 != 0 && (arg >> 8) & 0xf == 0,
                    _ => false,
                };
                if ok {
                    let val = (arg as u8 & 0x80) | (arg as u8 & 0x7f).min(AMP_STEPS);
                    let amp = if nid == NID_DAC { &mut self.st.dac_amp } else { &mut self.st.adc_amp };
                    if arg & 0x2000 != 0 {
                        amp[0] = val;
                    }
                    if arg & 0x1000 != 0 {
                        amp[1] = val;
                    }
                }
                0
            }
            // Get amp: bit 15 output, bit 13 left.
            0xb => {
                let output = arg & 0x8000 != 0;
                let fits = (nid == NID_DAC && output) || (nid == NID_ADC && !output);
                match self.amp(nid) {
                    Some(a) if fits => a[if arg & 0x2000 != 0 { 0 } else { 1 }] as u32,
                    _ => 0,
                }
            }
            _ => 0,
        }
    }
}

/// Scale factors (16.16 fixed point) for an amp's two channels.
pub fn gains(amp: [u8; 2]) -> [u32; 2] {
    amp.map(|a| {
        if a & 0x80 != 0 {
            return 0;
        }
        let db = (a & 0x7f).min(AMP_STEPS) as f64 - AMP_STEPS as f64;
        (10f64.powf(db / 20.0) * 65536.0) as u32
    })
}

/// Apply gains to interleaved S16LE frames of `ch` (1 or 2) channels.
pub fn apply_gains(pcm: &mut [u8], ch: usize, g: [u32; 2]) {
    if g == [65536; 2] {
        return;
    }
    for (k, s) in pcm.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let v = (i16::from_le_bytes(*s) as i64 * g[if ch == 2 { k % 2 } else { 0 }] as i64) >> 16;
        s.copy_from_slice(&(v as i16).to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(nid: u8, verb: u32, arg: u32) -> u32 {
        (nid as u32) << 20 | verb << 8 | arg
    }

    #[test]
    fn enumerates_like_a_codec() {
        let mut c = Codec::new(CodecIds { vendor: 0x10ec, device: 0x0887, subsys: 0x1043_8445 }, true);
        assert_eq!(c.verb(v(0, 0xf00, 0)), Some(0x10ec_0887));
        assert_eq!(c.verb(v(1, 0xf00, 4)), Some(2 << 16 | 4));
        assert_eq!(c.verb(v(1, 0xf20, 0)), Some(0x1043_8445));
        assert_eq!(c.verb(v(3, 0xf02, 0)), Some(2));
        assert_eq!(c.verb(v(5, 0xf1c, 0)), Some(CFG_MIC_PIN));
        assert_eq!(c.verb(1 << 28 | v(0, 0xf00, 0)), None, "no codec at address 1");
        assert_eq!(c.verb(v(0x7f, 0xf00, 9)), Some(0));
        let mut no_mic = Codec::new(CodecIds::default(), false);
        assert_eq!(no_mic.verb(v(1, 0xf00, 4)), Some(2 << 16 | 2));
        assert_eq!(no_mic.verb(v(5, 0xf00, 9)), Some(0));
        no_mic.verb(v(4, 0x706, 0x50));
        assert_eq!(no_mic.adc_tag(), 0);
    }

    #[test]
    fn amps_and_streams() {
        let mut c = Codec::new(CodecIds::default(), true);
        c.verb(v(2, 0x706, 0x10));
        assert_eq!(c.dac_tag(), 1);
        // Output amp, left only, gain way past the top: clamped.
        c.verb(2 << 20 | 3 << 16 | 0xa07f);
        assert_eq!(c.verb(2 << 20 | 0xb << 16 | 0xa000), Some(AMP_STEPS as u32));
        // Mute right.
        c.verb(2 << 20 | 3 << 16 | 0x90ca);
        assert_eq!(c.verb(2 << 20 | 0xb << 16 | 0x8000), Some(0x80 | AMP_STEPS as u32));
        assert_eq!(gains(c.st.dac_amp), [65536, 0]);
        let mut pcm = [0x00, 0x40, 0x00, 0x40];
        apply_gains(&mut pcm, 2, gains(c.st.dac_amp));
        assert_eq!(pcm, [0x00, 0x40, 0, 0]);
        // -6 dB is about half.
        let g = gains([AMP_STEPS - 6; 2])[0];
        assert!((32000..33500).contains(&g), "{g}");
        c.verb(v(1, 0x7ff, 0));
        assert_eq!(c.st, CodecState::default());
    }
}
