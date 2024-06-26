use crate::codegen::Compilation;
use crate::codegen::CompilationInfo;
use crate::codegen::CompilationSettings;
use crate::codegen::Compiler;
use crate::compute::Kernel;
use crate::compute::WorkGroup;
use crate::fusion::strides_dyn_rank;
use crate::fusion::JitFusionHandle;
use crate::kernel::SourceTemplate;
use crate::JitBackend;
use crate::Runtime;
use burn_compute::client::ComputeClient;
use burn_compute::server::Handle;
use burn_compute::tune::AutotuneOperation;
use burn_fusion::stream::Context;
use burn_fusion::{TensorDescription, TensorStatus};
use burn_tensor::Device;
use std::marker::PhantomData;
use std::sync::Arc;

use super::tracing::ExecutionInfo;

#[derive(new)]
pub struct FusionKernel<R: Runtime> {
    id: String, // Same ID for all different settings.
    info: Arc<CompilationInfo>,
    settings: CompilationSettings,
    runtime_info: Vec<OutputRuntimeInfo>,
    workgroup: WorkGroup,
    _runtime: PhantomData<R>,
}

pub trait FusionKernelFactory<R: Runtime> {
    /// Create a new kernel.
    fn create(
        &self,
        handles_inputs: &[JitFusionHandle<R>],
        inputs: &[&TensorDescription],
        outputs: &[&TensorDescription],
        stateful: bool, // Should be set to false when running autotune.
    ) -> FusionKernel<R>;
}

/// An instantiation of a [kernel](Kernel) that can be executed.
#[derive(new)]
pub struct ExecutableKernel<R: Runtime> {
    kernel: Box<dyn Kernel>,
    handles: Vec<Handle<R::Server>>,
    client: ComputeClient<R::Server, R::Channel>,
}

/// An instantiation of a [kernel](Kernel) that can be autotuned.
///
/// The main difference with an [executable kernel](ExecutableKernel) is that this kernel can be
/// cloned and executed multiple times to properly collect benchmarks.
///
/// The clone function used is defined in the trait [AutotuneOperation] instead of [Clone].
#[derive(new)]
pub struct AutotunableKernel<R: Runtime> {
    kernel: Arc<dyn Kernel>,
    handles: Vec<Handle<R::Server>>,
    client: ComputeClient<R::Server, R::Channel>,
}

// Information related to the output of this kernel.
#[derive(Debug)]
pub enum OutputRuntimeInfo {
    Inplace { input_index: usize },
    Array { size: usize },
}

impl<R: Runtime> ExecutableKernel<R> {
    /// Execute the kernel.
    pub fn execute(self) {
        self.client
            .execute(self.kernel, &self.handles.iter().collect::<Vec<_>>())
    }
}

impl<R: Runtime> AutotuneOperation for AutotunableKernel<R> {
    fn execute(self: Box<Self>) {
        self.client.execute(
            Box::new(self.kernel),
            &self.handles.iter().collect::<Vec<_>>(),
        )
    }

    fn clone(&self) -> Box<dyn AutotuneOperation> {
        Box::new(Self {
            kernel: self.kernel.clone(),
            handles: self.handles.iter().map(Clone::clone).collect(),
            client: self.client.clone(),
        })
    }
}

impl<R: Runtime> From<ExecutableKernel<R>> for AutotunableKernel<R> {
    fn from(value: ExecutableKernel<R>) -> Self {
        Self {
            kernel: Arc::new(value.kernel),
            handles: value.handles,
            client: value.client,
        }
    }
}

impl<R: Runtime> FusionKernel<R> {
    pub fn create<K: FusionKernelFactory<R>>(
        factory: &K,
        running_info: &ExecutionInfo<'_>,
        context: &mut Context<'_, JitBackend<R>>,
        device: Device<JitBackend<R>>,
        client: ComputeClient<R::Server, R::Channel>,
        stateful: bool,
    ) -> ExecutableKernel<R> {
        let (handles_input, inputs_description_updated, outputs_description_updated) =
            process_inputs_outputs(
                &running_info.inputs,
                &running_info.outputs,
                context,
                stateful,
            );

        let fusion_kernel = factory.create(
            &handles_input,
            &inputs_description_updated,
            &outputs_description_updated,
            stateful,
        );

        let rank_input = running_info
            .inputs
            .first()
            .map(|desc| desc.shape.len())
            .unwrap_or(1);
        let rank_output = running_info
            .outputs
            .first()
            .map(|desc| desc.shape.len())
            .unwrap_or(1);
        let rank = usize::max(rank_input, rank_output);

        let num_tensors = running_info.inputs.len() + running_info.outputs.len();
        // The buffer starts with the rank, then each tensor shape and stride.
        let info_size = (num_tensors * rank * 2) + 1;

        let mut num_handles = num_tensors + 1;
        if running_info.scalars.num_float > 0 {
            num_handles += 1;
        }
        if running_info.scalars.num_int > 0 {
            num_handles += 1;
        }

        let mut info = Vec::with_capacity(info_size);
        let mut handles = Vec::with_capacity(num_handles);
        let mut output_register = Vec::with_capacity(outputs_description_updated.len());

        // We register the info and handles for the inputs.
        for (handle, tensor) in handles_input.into_iter().zip(inputs_description_updated) {
            register_info_tensor(&mut info, tensor, &handle);
            handles.push(handle.handle);
        }

        // We register the info and handles for the outputs.
        for (tensor, output_info) in outputs_description_updated
            .into_iter()
            .zip(fusion_kernel.runtime_info.iter())
        {
            match output_info {
                // Use the input inplace for this output.
                OutputRuntimeInfo::Inplace { input_index } => {
                    let handle = handles.get(*input_index).unwrap().clone();
                    let handle_fusion = JitFusionHandle {
                        client: client.clone(),
                        device: device.clone(),
                        strides: strides_dyn_rank(&tensor.shape),
                        handle,
                    };
                    output_register.push((tensor.id, handle_fusion));
                }
                // Create a new buffer for this output.
                OutputRuntimeInfo::Array { size } => {
                    let handle_fusion = JitFusionHandle {
                        client: client.clone(),
                        device: device.clone(),
                        strides: strides_dyn_rank(&tensor.shape),
                        handle: client.empty(*size),
                    };

                    register_info_tensor(&mut info, tensor, &handle_fusion);
                    handles.push(handle_fusion.handle.clone());
                    output_register.push((tensor.id, handle_fusion));
                }
            };
        }

        // Create the info buffer.
        handles.push(client.create(bytemuck::cast_slice(&info)));

        // Finally we finish with the named bindings.
        if running_info.scalars.num_float > 0 {
            handles.push(client.create(bytemuck::cast_slice(
                &context.scalar_floats[0..running_info.scalars.num_float],
            )));
        }

        if running_info.scalars.num_int > 0 {
            handles.push(client.create(bytemuck::cast_slice(
                &context.scalar_ints[0..running_info.scalars.num_int],
            )));
        }

        // We have to register the output handles to the context.
        for (id, handle) in output_register {
            context.handles.register_handle(id, handle);
        }

        ExecutableKernel::new(Box::new(fusion_kernel), handles, client)
    }
}

impl<R: Runtime> Kernel for FusionKernel<R> {
    fn source(&self) -> SourceTemplate {
        log::info!("Compiling ... {:?}", self.id());
        let compiled = Compilation::new(self.info.as_ref().clone()).compile(self.settings.clone());
        let compiled = <R::Compiler as Compiler>::compile(compiled);

        SourceTemplate::new(compiled.to_string())
    }

    fn id(&self) -> String {
        format!("{}", self.settings) + self.id.as_str()
    }

    fn workgroup(&self) -> crate::compute::WorkGroup {
        self.workgroup.clone()
    }
}

fn register_info_tensor<R: Runtime>(
    info: &mut Vec<u32>,
    tensor: &TensorDescription,
    handle: &JitFusionHandle<R>,
) {
    if info.is_empty() {
        info.push(handle.strides.len() as u32);
    }

    for s in handle.strides.iter() {
        info.push(*s as u32);
    }
    for s in tensor.shape.iter() {
        info.push(*s as u32);
    }
}

fn process_inputs_outputs<'a, R: Runtime>(
    inputs: &[&TensorDescription],
    outputs: &[&TensorDescription],
    context: &'a mut Context<'_, JitBackend<R>>,
    stateful: bool,
) -> (
    Vec<JitFusionHandle<R>>,
    Vec<&'a TensorDescription>,
    Vec<&'a TensorDescription>,
) {
    let mut inputs_description_updated = Vec::with_capacity(inputs.len());
    let mut outputs_description_updated = Vec::with_capacity(outputs.len());
    let mut handles_input = Vec::new();

    for tensor in inputs.iter() {
        let status = if stateful {
            &tensor.status // Important to take the status of the relative graph and not
                           // the global graph, since the status of the global graph
                           // might be of a later operation on the same tensor id.
        } else {
            &TensorStatus::ReadOnly
        };

        let tensor = context.tensors.get(&tensor.id).unwrap();
        let handle = context.handles.get_handle(&tensor.id, status);

        handles_input.push(handle);
        inputs_description_updated.push(tensor);
    }

    for tensor in outputs.iter() {
        let tensor = context.tensors.get(&tensor.id).unwrap();
        outputs_description_updated.push(tensor);
    }

    (
        handles_input,
        inputs_description_updated,
        outputs_description_updated,
    )
}
