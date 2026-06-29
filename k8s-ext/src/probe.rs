use std::path::Path;

use super::*;

pub trait ProbeExt: Sized {
    fn http_get(path: impl AsRef<Path>, port: impl ToIntOrString) -> Self;
    fn failure_threshold(self, threshold: i32) -> Self;
    fn initial_delay_seconds(self, seconds: i32) -> Self;
    fn period_seconds(self, seconds: i32) -> Self;
}

impl ProbeExt for corev1::Probe {
    fn http_get(path: impl AsRef<Path>, port: impl ToIntOrString) -> Self {
        Self {
            http_get: Some(corev1::HTTPGetAction {
                path: Some(path.as_ref().display().to_string()),
                port: port.to_int_or_string(),
                scheme: Some("HTTP".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn failure_threshold(mut self, threshold: i32) -> Self {
        self.failure_threshold = Some(threshold);
        self
    }

    fn initial_delay_seconds(mut self, seconds: i32) -> Self {
        self.initial_delay_seconds = Some(seconds);
        self
    }

    fn period_seconds(mut self, seconds: i32) -> Self {
        self.period_seconds = Some(seconds);
        self
    }
}
