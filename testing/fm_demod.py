#!/usr/bin/env python3
"""
FM Demodulator for Complex IQ Samples

Reads a raw file containing complex IQ samples (f32) and demodulates
FM by computing the phase derivative (instantaneous frequency).
"""

import numpy as np
import wave
import struct
import argparse
from scipy import signal


def create_bandpass_filter(sample_rate, bandwidth_hz=15000, num_taps=101):
    """
    Create a Hann-windowed sinc bandpass filter.

    Args:
        sample_rate: Sample rate in Hz
        bandwidth_hz: Bandwidth in Hz (±bandwidth_hz from DC)
        num_taps: Number of filter taps (should be odd)

    Returns:
        Filter coefficients
    """
    # Normalize cutoff frequency to Nyquist frequency
    cutoff_normalized = 2. * bandwidth_hz / sample_rate

    # Create a lowpass filter using Hann-windowed sinc
    # This filters from -cutoff to +cutoff (symmetric around DC)
    taps = signal.firwin(num_taps, cutoff_normalized, window='hann')

    return taps


def apply_filter(iq_samples, filter_taps):
    """
    Apply FIR filter to complex IQ samples.

    Args:
        iq_samples: Complex numpy array of IQ samples
        filter_taps: Filter coefficients

    Returns:
        Filtered complex IQ samples
    """
    return np.convolve(iq_samples, filter_taps)


def demodulate_fm(iq_samples, sample_rate):
    """
    Demodulate FM from complex IQ samples.

    Args:
        iq_samples: Complex numpy array of IQ samples

    Returns:
        Numpy array of demodulated audio samples
    """
    # Compute the phase angle of each sample
    import matplotlib.pyplot as plt

    amp = np.abs(iq_samples)
    phase_diff = np.diff(np.atan2(np.imag(iq_samples), np.real(iq_samples)))
    phase_diff = (phase_diff + np.pi) % (2 * np.pi) - np.pi
    plt.plot(amp / np.max(amp))
    plt.plot(phase_diff)
    #phase_diff2 = np.diff(phase_diff)
    #phase_diff2 = (phase_diff + np.pi) % (2 * np.pi) - np.pi
    #phase_diff2 = np.cumsum(phase_diff2)
    #plt.plot(phase_diff2)
    plt.show()

    # The phase difference is proportional to the instantaneous frequency
    # Normalize to [-1, 1] range for audio
    if len(phase_diff) > 0:
        audio = phase_diff / np.max(np.abs(phase_diff))
    else:
        audio = phase_diff

    plt.plot( np.fft.fftshift(sample_rate * np.fft.fftfreq(len(audio))), np.fft.fftshift(20. * np.log10(np.abs(np.fft.fft(audio)))))
    plt.show()

    return audio

def demodulate_fm_hilbert(iq_samples):
    """
    Demodulate FM from complex IQ samples.

    Args:
        iq_samples: Complex numpy array of IQ samples

    Returns:
        Numpy array of demodulated audio samples
    """
    # Compute the phase angle of each sample
    phase = np.unwrap(np.angle(signal.hilbert(iq_samples)))

    # Compute the phase difference (derivative)
    phase_diff = np.diff(phase)

    # The phase difference is proportional to the instantaneous frequency
    # Normalize to [-1, 1] range for audio
    if len(phase_diff) > 0:
        audio = phase_diff / np.max(np.abs(phase_diff))
    else:
        audio = phase_diff

    return audio


def read_iq_samples(filename):
    """
    Read complex IQ samples from a raw file.
    Format: interleaved f32 (I, Q, I, Q, ...)

    Args:
        filename: Path to the raw IQ file

    Returns:
        Complex numpy array
    """
    # Read the raw f32 data
    data = np.fromfile(filename, dtype=np.float32)

    # Reshape into I/Q pairs and create complex array
    # Even indices are I (real), odd indices are Q (imaginary)
    iq = data[::2] + 1j * data[1::2]

    return iq


def write_wav(filename, audio_data, sample_rate):
    """
    Write audio data to a WAV file.

    Args:
        filename: Output WAV filename
        audio_data: Numpy array of audio samples in range [-1, 1]
        sample_rate: Sample rate in Hz
    """
    # Convert to 16-bit PCM
    audio_int16 = np.int16(audio_data * 32767)

    with wave.open(filename, 'w') as wav_file:
        # Set WAV parameters: 1 channel, 2 bytes per sample, sample rate
        wav_file.setnchannels(1)
        wav_file.setsampwidth(2)
        wav_file.setframerate(sample_rate)

        # Write audio data
        wav_file.writeframes(audio_int16.tobytes())


def main():
    parser = argparse.ArgumentParser(
        description='Demodulate FM from complex IQ samples'
    )
    parser.add_argument(
        'input_file',
        help='Input raw file containing complex f32 IQ samples'
    )
    parser.add_argument(
        '-o', '--output',
        default='output.wav',
        help='Output WAV file (default: output.wav)'
    )
    parser.add_argument(
        '-r', '--sample-rate',
        type=int,
        default=48000,
        help='Sample rate in Hz (default: 48000)'
    )
    #parser.add_argument(
    #    '-b', '--bandwidth',
    #    type=int,
    #    default=15000,
    #    help='Filter bandwidth in Hz (default: 15000 for ±15 KHz)'
    #)
    #parser.add_argument(
    #    '-t', '--taps',
    #    type=int,
    #    default=101,
    #    help='Number of filter taps (default: 101)'
    #)

    args = parser.parse_args()

    print(f"Reading IQ samples from {args.input_file}...")
    iq_samples = read_iq_samples(args.input_file)
    print(f"Loaded {len(iq_samples)} IQ samples")

    #print(f"Creating bandpass filter (±{args.bandwidth} Hz, {args.taps} taps)...")
    #filter_taps = create_bandpass_filter(args.sample_rate, args.bandwidth, args.taps)

    #print("Applying filter...")
    #filtered_iq = apply_filter(iq_samples, filter_taps)
    #print(f"Filtered {len(filtered_iq)} samples")

    #import matplotlib.pyplot as plt
    #plt.plot(np.fft.fftshift(20. * np.log10(np.abs(np.fft.fft(filtered_iq)))))
    #plt.show()

    print("Demodulating FM...")
    audio = demodulate_fm(iq_samples, args.sample_rate)
    print(f"Generated {len(audio)} audio samples")

    print(f"Writing WAV file to {args.output}...")
    write_wav(args.output, audio, args.sample_rate)
    print("Done!")


if __name__ == '__main__':
    main()
