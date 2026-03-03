#!/usr/bin/env python3
import sys
import numpy as np
import matplotlib.pyplot as plt

# Load IQ samples from the raw file
samples = np.fromfile(sys.argv[1], dtype=np.complex64)[0:100000]
#samples = np.fromfile('lpf_impulse_response.raw', dtype=np.complex64)

print(f"Loaded {len(samples)} samples")

plt.title("IQ Samples")
plt.plot(np.real(samples), label="I")
plt.plot(np.imag(samples), label="Q")
plt.show()

# Compute FFT
fft_result = np.fft.fft(samples)
fft_shifted = np.fft.fftshift(fft_result)

# Compute magnitude in dB
magnitude = np.abs(fft_shifted)
magnitude_db = 20 * np.log10(magnitude + 1e-10)  # Add small value to avoid log(0)

# Create frequency axis (normalized to sample rate)
freqs = np.fft.fftshift(20e6 * np.fft.fftfreq(len(samples)))

# Plot
plt.figure(figsize=(12, 6))
plt.plot(freqs, magnitude_db)
plt.xlabel('Normalized Frequency (fraction of sample rate)')
plt.ylabel('Magnitude (dB)')
plt.title(f'FFT Magnitude Spectrum ({len(samples)} samples)')
plt.grid(True)
plt.tight_layout()
plt.show()
