#[derive(Default)]
pub(super) struct LoadSampler {
    #[cfg(target_os = "windows")]
    counter: Option<windows::Counter>,
    #[cfg(target_os = "windows")]
    averages: [f64; 3],
}

impl LoadSampler {
    pub(super) fn capture(&mut self, _elapsed_secs: f64) -> [Option<f64>; 3] {
        #[cfg(target_os = "windows")]
        {
            if self.counter.is_none() {
                self.counter = windows::Counter::open();
            }
            let Some(value) = self.counter.as_ref().and_then(windows::Counter::value) else {
                return [None; 3];
            };
            smooth(&mut self.averages, value, _elapsed_secs);
            self.averages.map(|value| Some(crate::util::round2(value)))
        }
        #[cfg(unix)]
        {
            let load = sysinfo::System::load_average();
            [load.one, load.five, load.fifteen]
                .map(|value| value.is_finite().then_some(crate::util::round2(value)))
        }
        #[cfg(not(any(unix, target_os = "windows")))]
        {
            [None; 3]
        }
    }
}

#[cfg(any(target_os = "windows", test))]
fn smooth(averages: &mut [f64; 3], value: f64, elapsed_secs: f64) {
    for (average, seconds) in averages.iter_mut().zip([60.0, 300.0, 900.0]) {
        *average += (value - *average) * (1.0 - (-elapsed_secs / seconds).exp());
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use windows_sys::Win32::System::Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
        PdhOpenQueryW, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE,
        PDH_HCOUNTER, PDH_HQUERY,
    };

    pub(super) struct Counter {
        query: PDH_HQUERY,
        counter: PDH_HCOUNTER,
    }

    // The sampler owns the query and never shares it between threads.
    unsafe impl Send for Counter {}

    impl Counter {
        pub(super) fn open() -> Option<Self> {
            let mut query = std::ptr::null_mut();
            if unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) } != 0 {
                return None;
            }
            let mut counter = std::ptr::null_mut();
            let path: Vec<u16> = "\\System\\Processor Queue Length"
                .encode_utf16().chain(Some(0)).collect();
            if unsafe { PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) } != 0 {
                unsafe { PdhCloseQuery(query) };
                return None;
            }
            Some(Self { query, counter })
        }

        pub(super) fn value(&self) -> Option<f64> {
            let mut value: PDH_FMT_COUNTERVALUE = unsafe { std::mem::zeroed() };
            if unsafe { PdhCollectQueryData(self.query) } != 0
                || unsafe { PdhGetFormattedCounterValue(self.counter, PDH_FMT_DOUBLE, std::ptr::null_mut(), &mut value) } != 0
                || ![PDH_CSTATUS_VALID_DATA, PDH_CSTATUS_NEW_DATA].contains(&value.CStatus)
            {
                return None;
            }
            let value = unsafe { value.Anonymous.doubleValue };
            (value.is_finite() && value >= 0.0).then_some(value)
        }
    }

    impl Drop for Counter {
        fn drop(&mut self) {
            unsafe { PdhCloseQuery(self.query) };
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn smoothing_uses_elapsed_time_and_load_units() {
        let mut averages = [0.0; 3];
        super::smooth(&mut averages, 2.0, 60.0);
        assert!((averages[0] - 2.0 * (1.0 - (-1.0_f64).exp())).abs() < 1e-10);
        assert!(averages[0] > averages[1] && averages[1] > averages[2]);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_counter_uses_the_available_processor_queue() {
        let counter = super::windows::Counter::open().expect("Processor Queue Length counter");
        assert!(counter.value().is_some());
    }
}
