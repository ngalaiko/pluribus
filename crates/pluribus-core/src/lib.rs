//! Kernel contracts for Pluribus.

pub mod authority;
pub mod blob;
pub mod delivery;
pub mod event;
pub mod http;
pub mod identity;
pub mod net;
pub mod registry;
pub mod secret;
pub mod state;
pub mod stream;

pub use authority::{
    Audience, Authority, AuthorityId, CapabilityName, ConstraintPolicy, ConstraintSet, Decision,
    DenialReason, Grant, Origin, OriginKind,
};
pub use blob::{
    BlobChunk, BlobError, BlobStore, BlobUploadId, InMemoryBlobStore, SHA256_ALGORITHM,
    validate_blob_ref, validate_media_type,
};
pub use delivery::{CursorKey, DeliveryCommit, DeliveryError, DeliveryReceipt, DeliveryStore};
pub use event::{
    AppendError, AppendRequest, BlobRef, CommittedEvent, EventId, EventMetadataSource,
    EventPayload, EventQuery, EventStore, StreamId, StreamKind,
};
pub use http::{
    HttpError, HttpFrame, HttpFramePage, HttpGrant, HttpHeader, HttpRequest, HttpResponse,
    HttpService, HttpStreamProtocol, HttpStreamService, HttpStreamingResponse,
};
pub use identity::{PrincipalId, PrincipalKind, PrincipalRef};
pub use net::is_public_address;
pub use registry::{
    EventTypeRegistry, PLUGIN_EVENT_TYPES, RESERVED_EVENT_TYPES, RegistryError, matches_pattern,
};
pub use secret::{InMemoryCredentialStore, PluginCredentialStore, SecretError, SecretHandle};
pub use state::{
    StateEntry, StateError, StateMutation, StateNamespace, StatePage, StateSnapshot, StateStore,
};
pub use stream::{StartTls, StreamEndpoint, StreamError, StreamGrant, StreamPage, StreamService};
