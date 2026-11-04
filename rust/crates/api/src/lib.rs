//! The two services this program talks to: TypeSafe, which decides, and OpenAI, which writes,
//! reads screenshots and transcribes speech.

pub mod anthropic;
pub mod chat;
pub mod deepgram;
pub mod murf;
pub mod rumik;
pub mod openai;
pub mod typesafe;
