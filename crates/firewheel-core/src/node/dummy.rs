use core::any::Any;

use crate::{
    channel_config::ChannelConfig,
    collector::OwnedGc,
    event::ProcEvents,
    node::{ProcBuffers, ProcExtra},
};

use super::{
    AudioNode, AudioNodeInfo, AudioNodeProcessor, ConstructProcessorContext, ProcInfo,
    ProcessStatus,
};

/// A "dummy" [`AudioNode`], a node which does nothing.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DummyNode;

/// The configuration for a [`DummyNode`], a node which does nothing.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DummyNodeConfig {
    pub channel_config: ChannelConfig,
}

impl AudioNode for DummyNode {
    type Configuration = DummyNodeConfig;

    fn info(&self, config: &Self::Configuration) -> AudioNodeInfo {
        AudioNodeInfo::new()
            .debug_name("dummy")
            .channel_config(config.channel_config)
    }

    fn construct_processor(
        &self,
        _config: &Self::Configuration,
        _cx: ConstructProcessorContext,
    ) -> impl AudioNodeProcessor<Self> {
        DummyProcessor
    }
}

struct DummyProcessor;

impl AudioNodeProcessor<DummyNode> for DummyProcessor {
    fn process(
        &mut self,
        _info: &ProcInfo,
        _buffers: ProcBuffers,
        //TODO
        _events: &mut ProcEvents<DummyNode>,
        _extra: &mut ProcExtra,
    ) -> ProcessStatus {
        ProcessStatus::Bypass
    }
}
