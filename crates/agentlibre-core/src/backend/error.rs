//! What can go wrong reaching the model's server: the connection, its
//! status and reply, its certificate, a missing projector (AGL-121, D66).

use std::path::PathBuf;

use crate::i18n::{Lang, Msg, server};
use crate::settings::ServerKind;
use crate::tr;

/// The backend's own error, wrapped by the core's `Error::Backend`
/// (AGL-121, D66).
#[derive(Debug)]
pub enum Error {
    /// The server of `kind` cannot be reached (D97; AGL-038, D1).
    ServerUnreachable {
        kind: ServerKind,
        url: String,
        detail: String,
    },
    /// The server of `kind` answered with an error status.
    ServerStatus {
        kind: ServerKind,
        url: String,
        status: u16,
        message: String,
    },
    /// The server of `kind` answered something agl does not understand.
    ServerReply {
        kind: ServerKind,
        url: String,
        detail: String,
    },
    /// The file of `private_ca` cannot be used (AGL-058, D12).
    ServerCa { path: PathBuf, detail: String },
    /// The server's certificate is not trusted (AGL-058, D12); `key` is the
    /// setting that names a private CA for that server (AGL-121, D4).
    ServerCertificate { url: String, detail: String, key: &'static str },
    /// The prompt names a medium but the server has no multimodal
    /// projector (AGL-014).
    NoMultimodal,
    /// The server generated text the grammar does not allow.
    OutsideGrammar(String),
}

impl Error {
    /// The message of `self` in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::ServerUnreachable { kind, url, detail } => tr!(
                lang,
                SERVER_UNREACHABLE,
                server = server::name(*kind, lang),
                url = url,
                detail = detail
            ),
            Self::ServerStatus {
                kind,
                url,
                status,
                message,
            } => {
                tr!(
                    lang,
                    SERVER_STATUS,
                    server = server::name(*kind, lang),
                    url = url,
                    status = status,
                    message = message
                )
            }
            Self::ServerReply { kind, url, detail } => tr!(
                lang,
                SERVER_REPLY,
                server = server::name(*kind, lang),
                url = url,
                detail = detail
            ),
            Self::ServerCa { path, detail } => {
                tr!(lang, SERVER_CA, path = path.display(), detail = detail)
            }
            Self::ServerCertificate { url, detail, key } => {
                tr!(lang, SERVER_CERTIFICATE, url = url, detail = detail, key = key)
            }
            Self::NoMultimodal => tr!(lang, NO_MULTIMODAL).to_string(),
            Self::OutsideGrammar(text) => tr!(lang, OUTSIDE_GRAMMAR, text = text),
        }
    }
}

/// `Error::NoMultimodal` (AGL-014).
const NO_MULTIMODAL: Msg = Msg {
    en: "the server has no multimodal projector, so it cannot take an image; set `mmproj` in the model of the Function and restart it (agl server stop)",
    uk: "у сервера немає мультимодального проєктора, тож він не приймає зображень; задай `mmproj` у моделі Функції і перезапусти його (agl server stop)",
};

/// `Error::ServerCa` (AGL-058, D12).
const SERVER_CA: Msg = Msg {
    en: "the CA file {path} of `private_ca` cannot be used: {detail}",
    uk: "файл CA {path} із `private_ca` не можна використати: {detail}",
};

/// `Error::ServerCertificate` (AGL-058, D12).
const SERVER_CERTIFICATE: Msg = Msg {
    en: "the certificate of the server at {url} is not trusted: {detail}\nIf a private CA signs it, name its certificate in `{key}`",
    uk: "сертифікату сервера за {url} немає довіри: {detail}\nЯкщо його підписує приватний CA, вкажи сертифікат CA у `{key}`",
};

/// `Error::ServerReply`.
const SERVER_REPLY: Msg = Msg {
    en: "unexpected reply from {server} at {url}: {detail}",
    uk: "неочікувана відповідь: {server} за адресою {url}: {detail}",
};

/// `Error::ServerStatus`.
const SERVER_STATUS: Msg = Msg {
    en: "{server} at {url} answered {status}: {message}",
    uk: "{server} за адресою {url} відповів {status}: {message}",
};

/// `Error::ServerUnreachable`.
const SERVER_UNREACHABLE: Msg = Msg {
    en: "cannot reach {server} at {url}: {detail}\nTo start it: agl server start",
    uk: "не вдалося з'єднатися з {server} за адресою {url}: {detail}\nЩоб запустити: agl server start",
};

/// `Error::OutsideGrammar`.
const OUTSIDE_GRAMMAR: Msg = Msg {
    en: "llama-server generated text outside the grammar: {text:?}",
    uk: "llama-server згенерував текст поза граматикою: {text:?}",
};
