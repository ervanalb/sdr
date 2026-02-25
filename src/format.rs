pub fn format_freq(freq: f64, precision: i32) -> String {
    if freq < 0. {
        format!("XXX Hz")
    } else if freq < 1e3 {
        format!("{:.*} Hz", (0 - precision).max(0) as usize, freq)
    } else if freq < 1e6 {
        format!("{:.*} kHz", (3 - precision).max(0) as usize, freq * 1e-3)
    } else if freq < 1e9 {
        format!("{:.*} MHz", (6 - precision).max(0) as usize, freq * 1e-6)
    } else if freq < 1e12 {
        format!("{:.*} GHz", (9 - precision).max(0) as usize, freq * 1e-9)
    } else {
        format!("XXX Hz")
    }
}

pub fn format_time(time: f64, precision: i32) -> String {
    format!("{:.*} s", (-precision).max(0) as usize, time)
}
