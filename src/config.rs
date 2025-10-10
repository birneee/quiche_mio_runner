use quiche_endpoint::Endpoint;
use crate::runner::Runner;

pub struct Config<TConnAppData, TAppData, TExternalEventValue> {
    pub pre_handle_recvs: fn(&mut Runner<TConnAppData, TAppData,TExternalEventValue>),
    /// executes once a batch of received QUIC packets are processed;
    /// executes before outgoing QUIC packets are generated;
    pub post_handle_recvs: fn(&mut Runner<TConnAppData, TAppData, TExternalEventValue>),
    pub on_external_event: Option<fn(&mut Endpoint<TConnAppData, TAppData>, &TExternalEventValue)>
}

impl<TConnAppData, TAppData, TExternalEventValue> Default for Config<TConnAppData, TAppData, TExternalEventValue> {
    fn default() -> Self {
        Config {
            pre_handle_recvs: |_| {},
            post_handle_recvs: |_| {},
            on_external_event: None
        }
    }
}
