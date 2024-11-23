use std::collections::VecDeque;

use bevy::{
    app::{App, First, Main, MainSchedulePlugin, PluginsState, Update},
    asset::{AssetId, AssetPlugin, AssetServer, Assets, Handle},
    core::{FrameCountPlugin, TaskPoolPlugin, TypeRegistrationPlugin},
    core_pipeline::CorePipelinePlugin,
    diagnostic::LogDiagnosticsPlugin,
    ecs::{
        entity::EntityHashMap,
        event::{event_update_condition, event_update_system, EventUpdates},
        query::QuerySingleError,
        schedule::ScheduleLabel,
        world,
    },
    gltf::GltfAssetLabel,
    log::{debug, error, info, LogPlugin},
    math::{UVec2, Vec3},
    pbr::{DirectionalLight, MeshMaterial3d, PbrPlugin, StandardMaterial},
    prelude::{
        AppTypeRegistry, Camera, Camera3d, Commands, Component, Deref, DerefMut,
        DespawnRecursiveExt, Entity, Event, EventReader, EventWriter, FromWorld, Image,
        ImagePlugin, IntoSystemConfigs, Mesh, Mesh3d, NonSendMut, PluginGroup, Query, Res, ResMut,
        Resource, Transform, With, World,
    },
    render::{
        camera::RenderTarget,
        pipelined_rendering::PipelinedRenderingPlugin,
        render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages},
        renderer::RenderDevice,
        view::{GpuCulling, RenderLayers},
        Extract, ExtractSchedule, RenderApp, RenderPlugin,
    },
    scene::{InstanceId, Scene, SceneInstance, SceneRoot, SceneSpawner},
    time::TimePlugin,
    ui::{IsDefaultUiCamera, TargetCamera},
    utils::{Entry, HashMap, HashSet},
    window::{WindowClosing, WindowCreated, WindowPlugin, WindowResized},
    winit::WinitPlugin,
    DefaultPlugins, MinimalPlugins,
};

use crate::PreviewAsset;

pub const BASE_PREVIEW_LAYER: usize = 128;
pub const PREVIEW_LAYERS_COUNT: usize = 8;
pub const PREVIEW_RENDER_FRAMES: u32 = 8;

#[derive(Resource)]
pub struct PreviewSettings {
    pub resolution: UVec2,
}

impl Default for PreviewSettings {
    fn default() -> Self {
        Self {
            resolution: UVec2::splat(256),
        }
    }
}

fn create_prerender_target(settings: &PreviewSettings) -> Image {
    let mut image = Image::new_fill(
        Extent3d {
            width: settings.resolution.x,
            height: settings.resolution.y,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &[0, 0, 0, 0],
        TextureFormat::Bgra8UnormSrgb,
        Default::default(),
    );

    image.texture_descriptor.usage |=
        TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST | TextureUsages::RENDER_ATTACHMENT;

    image
}

#[derive(Component)]
pub struct PreviewRenderView {
    pub layer: usize,
}

#[derive(Component, Default)]
pub struct PreviewRenderedFrames {
    pub cur_frame: u32,
}

#[derive(Event)]
pub struct PreviewRendered {
    pub layer: usize,
}

#[derive(Resource)]
pub struct PreviewSceneState {
    available_layers: u8,
    cameras: [Entity; PREVIEW_LAYERS_COUNT],
    lights: [Entity; PREVIEW_LAYERS_COUNT],
    scene_handles: [Handle<Scene>; PREVIEW_LAYERS_COUNT],
    scene_instances: [Option<InstanceId>; PREVIEW_LAYERS_COUNT],
    applied_layer: [bool; PREVIEW_LAYERS_COUNT],
    render_targets: [Handle<Image>; PREVIEW_LAYERS_COUNT],
}

impl Default for PreviewSceneState {
    fn default() -> Self {
        Self {
            available_layers: !0,
            cameras: [Entity::PLACEHOLDER; PREVIEW_LAYERS_COUNT],
            lights: [Entity::PLACEHOLDER; PREVIEW_LAYERS_COUNT],
            scene_handles: Default::default(),
            scene_instances: Default::default(),
            applied_layer: Default::default(),
            render_targets: Default::default(),
        }
    }
}

impl PreviewSceneState {
    pub fn occupy(
        &mut self,
        handle: Handle<Scene>,
        instance: InstanceId,
        render_target: Handle<Image>,
        commands: &mut Commands,
    ) {
        if self.is_full() {
            return;
        }

        let layer = self.available_layers.trailing_zeros() as usize;
        self.available_layers &= !(1 << layer);

        self.lights[layer] = commands
            .spawn((
                DirectionalLight::default(),
                Transform::IDENTITY.looking_to(Vec3::new(1.0, -1.0, 1.0), Vec3::Y),
                RenderLayers::from_layers(&[layer + BASE_PREVIEW_LAYER]),
            ))
            .id();
        self.cameras[layer] = commands
            .spawn((
                Camera3d::default(),
                Camera {
                    target: RenderTarget::Image(render_target.clone()),
                    ..Default::default()
                },
                Transform::from_translation(Vec3::new(-5.0, 2.0, -5.0))
                    .looking_at(Vec3::ZERO, Vec3::Y),
                RenderLayers::from_layers(&[layer + BASE_PREVIEW_LAYER]),
                PreviewRenderView { layer },
                PreviewRenderedFrames::default(),
            ))
            .id();
        self.render_targets[layer] = render_target;
        self.scene_handles[layer] = handle;
        self.scene_instances[layer] = Some(instance);
    }

    pub fn free(&mut self, layer: usize, commands: &mut Commands) {
        self.available_layers |= 1 << layer;
        commands.entity(self.lights[layer]).despawn();
        commands.entity(self.cameras[layer]).despawn();
        self.applied_layer[layer] = false;
        self.scene_instances[layer] = None;
    }

    pub fn is_full(&self) -> bool {
        self.available_layers.trailing_zeros() == PREVIEW_LAYERS_COUNT as u32
    }
}

/// Scenes that are rendered for preview purpose. This should be inserted into
/// main world.
#[derive(Resource, Default)]
pub struct PrerenderedScenes {
    rendered: HashMap<AssetId<Scene>, Handle<Image>>,
    rendering: HashSet<AssetId<Scene>>,
    queue: HashSet<Handle<Scene>>,
}

impl PrerenderedScenes {
    pub fn get_or_schedule(&mut self, handle: Handle<Scene>) -> Option<Handle<Image>> {
        let id = handle.id();
        match self.rendered.entry(id) {
            Entry::Occupied(e) => Some(e.get().clone()),
            Entry::Vacant(_) => {
                if !self.rendering.contains(&id) {
                    self.queue.insert(handle);
                    self.rendering.insert(id);
                }
                None
            }
        }
    }
}

pub(crate) fn update_queue(
    mut commands: Commands,
    mut prerendered: ResMut<PrerenderedScenes>,
    mut scene_spawner: ResMut<SceneSpawner>,
    mut scene_state: ResMut<PreviewSceneState>,
    settings: Res<PreviewSettings>,
    mut images: ResMut<Assets<Image>>,
    mut preview_rendered: EventReader<PreviewRendered>,
) {
    while !scene_state.is_full() {
        let Some(handle) = prerendered.queue.iter().nth(0).cloned() else {
            break;
        };
        prerendered.queue.remove(&handle);

        let instance = scene_spawner.spawn(handle.clone());
        let render_target = images.add(create_prerender_target(&settings));
        info!("Generating preview image for {:?}", handle);
        scene_state.occupy(handle, instance, render_target, &mut commands);
    }

    for finished in preview_rendered.read() {
        let scene_handle = scene_state.scene_handles[finished.layer].clone();
        prerendered.rendering.remove(&scene_handle.id());
        let render_target = scene_state.render_targets[finished.layer].clone();
        prerendered
            .rendered
            .insert(scene_handle.id(), render_target);
        info!("Preview image for {:?} generated.", scene_handle);

        let instance = scene_state.scene_instances[finished.layer].unwrap();
        scene_spawner.despawn_instance(instance);
        scene_state.free(finished.layer, &mut commands);
    }
}

pub(crate) fn update_preview_frames_counter(
    mut commands: Commands,
    mut counters_query: Query<(Entity, &mut PreviewRenderedFrames, &PreviewRenderView)>,
    mut preview_rendered: EventWriter<PreviewRendered>,
    scene_state: Res<PreviewSceneState>,
    scene_spawner: Res<SceneSpawner>,
) {
    for (entity, mut cnt, view) in &mut counters_query {
        if scene_state.scene_instances[view.layer]
            .is_some_and(|inst| scene_spawner.instance_is_ready(inst))
        {
            cnt.cur_frame += 1;

            if cnt.cur_frame >= PREVIEW_RENDER_FRAMES {
                commands.entity(entity).remove::<PreviewRenderedFrames>();
                preview_rendered.send(PreviewRendered { layer: view.layer });
            }
        }
    }
}

pub(crate) fn change_render_layers(
    mut commands: Commands,
    mut scene_state: ResMut<PreviewSceneState>,
    scene_spawner: Res<SceneSpawner>,
) {
    for layer in 0..PREVIEW_LAYERS_COUNT {
        if let Some(instance) = scene_state.scene_instances[layer] {
            if !scene_state.applied_layer[layer] && scene_spawner.instance_is_ready(instance) {
                scene_state.applied_layer[layer] = true;

                commands.insert_batch(
                    scene_spawner
                        .iter_instance_entities(instance)
                        .map(|e| (e, RenderLayers::from_layers(&[layer + BASE_PREVIEW_LAYER])))
                        .collect::<Vec<_>>(),
                );
            }
        }
    }
}
