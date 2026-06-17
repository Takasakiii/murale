use thiserror::Error;

#[derive(Error, Debug)]
pub enum MuraleError {
    #[error("egl: {0}")]
    Egl(String),

    #[error("mpv: {0}")]
    Mpv(#[from] libmpv2::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<khronos_egl::Error> for MuraleError {
    fn from(e: khronos_egl::Error) -> Self {
        MuraleError::Egl(format!("{e:?}"))
    }
}
