//! Contains all methods and structures to create and manage scene graphs.
//!
//! Scene graph is the foundation of the engine. Graph is a hierarchical data
//! structure where each element called node. Each node can have zero to one parent
//! node, and any children nodes. Node with no parent node called root, with no
//! children nodes - leaf. Graphical representation can be something like this:
//!
//! ```text
//!     Root____
//!       |    |
//!       D    A___
//!       |    |  |
//!       E    C  B
//!     ............
//! ```
//!
//! This picture clearly shows relations between nodes. Such structure allows us
//! to create scenes of any complexity by just linking nodes with each other.
//! Connections between nodes are used to traverse tree, to calculate global
//! transforms, global visibility and many other things. Most interesting here -
//! is global transform calculation - it allows you to produce complex movements
//! just by linking nodes to each other. Good example of this is skeleton which
//! is used in skinning (animating 3d model by set of bones).

use crate::{
    asset::ResourceState,
    core::{
        algebra::{
            Isometry3, Matrix4, Point3, Rotation3, Translation3, UnitQuaternion, Vector2, Vector3,
        },
        arrayvec::ArrayVec,
        color::Color,
        math::{aabb::AxisAlignedBoundingBox, frustum::Frustum, Matrix4Ext},
        pool::{
            Handle, Pool, PoolIterator, PoolIteratorMut, PoolPairIterator, PoolPairIteratorMut,
            Ticket,
        },
        visitor::{Visit, VisitResult, Visitor},
        VecExtensions,
    },
    physics3d::rapier::{
        dynamics::{
            CCDSolver, IntegrationParameters, IslandManager, JointSet, RigidBodyBuilder,
            RigidBodyHandle, RigidBodySet,
        },
        geometry::{
            self, BroadPhase, ColliderBuilder, ColliderHandle, ColliderSet, InteractionGroups,
            NarrowPhase, Ray, TriMesh,
        },
        pipeline::{EventHandler, PhysicsPipeline, QueryPipeline},
    },
    resource::model::NodeMapping,
    scene::{
        base::PropertyValue,
        collider::{ColliderChanges, ColliderShape},
        debug::SceneDrawingContext,
        joint::JointChanges,
        node::Node,
        rigidbody::RigidBodyChanges,
        transform::TransformBuilder,
        visibility::VisibilityCache,
    },
    utils::log::{Log, MessageKind},
};
use fxhash::FxHashMap;
use std::{
    cell::{Cell, RefCell},
    cmp::Ordering,
    fmt::{Debug, Display, Formatter},
    ops::{Index, IndexMut},
    time::Duration,
};

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum FeatureId {
    /// Shape-dependent identifier of a vertex.
    Vertex(u32),
    /// Shape-dependent identifier of an edge.
    Edge(u32),
    /// Shape-dependent identifier of a face.
    Face(u32),
    /// Unknown identifier.
    Unknown,
}

impl From<geometry::FeatureId> for FeatureId {
    fn from(v: geometry::FeatureId) -> Self {
        match v {
            geometry::FeatureId::Vertex(v) => FeatureId::Vertex(v),
            geometry::FeatureId::Edge(v) => FeatureId::Edge(v),
            geometry::FeatureId::Face(v) => FeatureId::Face(v),
            geometry::FeatureId::Unknown => FeatureId::Unknown,
        }
    }
}

/// Performance statistics for the physics part of the engine.
#[derive(Debug, Default, Clone)]
pub struct PhysicsPerformanceStatistics {
    /// A time that was needed to perform a single simulation step.
    pub step_time: Duration,

    /// A time that was needed to perform all ray casts.
    pub total_ray_cast_time: Cell<Duration>,
}

impl Display for PhysicsPerformanceStatistics {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Physics Step Time: {:?}\nPhysics Ray Cast Time: {:?}",
            self.step_time,
            self.total_ray_cast_time.get(),
        )
    }
}

impl PhysicsPerformanceStatistics {
    /// Resets performance statistics to default values.
    pub fn reset(&mut self) {
        *self = Default::default();
    }
}

/// A ray intersection result.
#[derive(Debug, Clone)]
pub struct Intersection {
    /// A handle of the collider with which intersection was detected.
    pub collider: Handle<Node>,

    /// A normal at the intersection position.
    pub normal: Vector3<f32>,

    /// A position of the intersection in world coordinates.
    pub position: Point3<f32>,

    /// Additional data that contains a kind of the feature with which
    /// intersection was detected as well as its index.
    ///
    /// # Important notes.
    ///
    /// FeatureId::Face might have index that is greater than amount of triangles in
    /// a triangle mesh, this means that intersection was detected from "back" side of
    /// a face. To "fix" that index, simply subtract amount of triangles of a triangle
    /// mesh from the value.
    pub feature: FeatureId,

    /// Distance from the ray origin.
    pub toi: f32,
}

/// A set of options for the ray cast.
pub struct RayCastOptions {
    /// A ray origin.
    pub ray_origin: Point3<f32>,

    /// A ray direction. Can be non-normalized.
    pub ray_direction: Vector3<f32>,

    /// Maximum distance of cast.
    pub max_len: f32,

    /// Groups to check.
    pub groups: InteractionGroups,

    /// Whether to sort intersections from closest to farthest.
    pub sort_results: bool,
}

/// A trait for ray cast results storage. It has two implementations: Vec and ArrayVec.
/// Latter is needed for the cases where you need to avoid runtime memory allocations
/// and do everything on stack.
pub trait QueryResultsStorage {
    /// Pushes new intersection in the storage. Returns true if intersection was
    /// successfully inserted, false otherwise.
    fn push(&mut self, intersection: Intersection) -> bool;

    /// Clears the storage.
    fn clear(&mut self);

    /// Sorts intersections by given compare function.
    fn sort_intersections_by<C: FnMut(&Intersection, &Intersection) -> Ordering>(&mut self, cmp: C);
}

impl QueryResultsStorage for Vec<Intersection> {
    fn push(&mut self, intersection: Intersection) -> bool {
        self.push(intersection);
        true
    }

    fn clear(&mut self) {
        self.clear()
    }

    fn sort_intersections_by<C>(&mut self, cmp: C)
    where
        C: FnMut(&Intersection, &Intersection) -> Ordering,
    {
        self.sort_by(cmp);
    }
}

impl<const CAP: usize> QueryResultsStorage for ArrayVec<Intersection, CAP> {
    fn push(&mut self, intersection: Intersection) -> bool {
        self.try_push(intersection).is_ok()
    }

    fn clear(&mut self) {
        self.clear()
    }

    fn sort_intersections_by<C>(&mut self, cmp: C)
    where
        C: FnMut(&Intersection, &Intersection) -> Ordering,
    {
        self.sort_by(cmp);
    }
}

pub struct PhysicsWorld {
    /// Current physics pipeline.
    pipeline: PhysicsPipeline,
    /// Current gravity vector. Default is (0.0, -9.81, 0.0)
    gravity: Vector3<f32>,
    /// A set of parameters that define behavior of every rigid body.
    integration_parameters: IntegrationParameters,
    /// Broad phase performs rough intersection checks.
    broad_phase: BroadPhase,
    /// Narrow phase is responsible for precise contact generation.
    narrow_phase: NarrowPhase,
    /// A continuous collision detection solver.
    ccd_solver: CCDSolver,
    /// Structure responsible for maintaining the set of active rigid-bodies, and putting non-moving
    /// rigid-bodies to sleep to save computation times.
    islands: IslandManager,

    /// A container of rigid bodies.
    bodies: RigidBodySet,

    /// A container of colliders.
    colliders: ColliderSet,

    /// A container of joints.
    joints: JointSet,

    /// Event handler collects info about contacts and proximity events.
    event_handler: Box<dyn EventHandler>,

    query: RefCell<QueryPipeline>,

    /// Performance statistics of a single simulation step.
    pub performance_statistics: PhysicsPerformanceStatistics,
}

impl PhysicsWorld {
    /// Creates a new instance of the physics world.
    fn new() -> Self {
        Self {
            pipeline: PhysicsPipeline::new(),
            gravity: Vector3::new(0.0, -9.81, 0.0),
            integration_parameters: IntegrationParameters::default(),
            broad_phase: BroadPhase::new(),
            narrow_phase: NarrowPhase::new(),
            ccd_solver: CCDSolver::new(),
            islands: IslandManager::new(),
            bodies: RigidBodySet::new(),
            colliders: ColliderSet::new(),
            joints: JointSet::new(),
            event_handler: Box::new(()),
            query: RefCell::new(Default::default()),
            performance_statistics: Default::default(),
        }
    }

    fn update(&mut self) {
        self.pipeline.step(
            &self.gravity,
            &self.integration_parameters,
            &mut self.islands,
            &mut self.broad_phase,
            &mut self.narrow_phase,
            &mut self.bodies,
            &mut self.colliders,
            &mut self.joints,
            &mut self.ccd_solver,
            &(),
            &*self.event_handler,
        );
    }

    /// Draws physics world. Very useful for debugging, it allows you to see where are
    /// rigid bodies, which colliders they have and so on.
    pub fn draw(&self, context: &mut SceneDrawingContext) {
        for (_, body) in self.bodies.iter() {
            context.draw_transform(body.position().to_homogeneous());
        }

        for (_, collider) in self.colliders.iter() {
            let body = self.bodies.get(collider.parent().unwrap()).unwrap();
            let collider_local_transform = collider.position_wrt_parent().unwrap().to_homogeneous();
            let transform = body.position().to_homogeneous() * collider_local_transform;
            if let Some(trimesh) = collider.shape().as_trimesh() {
                let trimesh: &TriMesh = trimesh;
                for triangle in trimesh.triangles() {
                    let a = transform.transform_point(&triangle.a);
                    let b = transform.transform_point(&triangle.b);
                    let c = transform.transform_point(&triangle.c);
                    context.draw_triangle(
                        a.coords,
                        b.coords,
                        c.coords,
                        Color::opaque(200, 200, 200),
                    );
                }
            } else if let Some(cuboid) = collider.shape().as_cuboid() {
                let min = -cuboid.half_extents;
                let max = cuboid.half_extents;
                context.draw_oob(
                    &AxisAlignedBoundingBox::from_min_max(min, max),
                    transform,
                    Color::opaque(200, 200, 200),
                );
            } else if let Some(ball) = collider.shape().as_ball() {
                context.draw_sphere(
                    body.position().translation.vector,
                    10,
                    10,
                    ball.radius,
                    Color::opaque(200, 200, 200),
                );
            } else if let Some(cone) = collider.shape().as_cone() {
                context.draw_cone(
                    10,
                    cone.radius,
                    cone.half_height * 2.0,
                    transform,
                    Color::opaque(200, 200, 200),
                );
            } else if let Some(cylinder) = collider.shape().as_cylinder() {
                context.draw_cylinder(
                    10,
                    cylinder.radius,
                    cylinder.half_height * 2.0,
                    true,
                    transform,
                    Color::opaque(200, 200, 200),
                );
            } else if let Some(round_cylinder) = collider.shape().as_round_cylinder() {
                context.draw_cylinder(
                    10,
                    round_cylinder.base_shape.radius,
                    round_cylinder.base_shape.half_height * 2.0,
                    false,
                    transform,
                    Color::opaque(200, 200, 200),
                );
            } else if let Some(triangle) = collider.shape().as_triangle() {
                context.draw_triangle(
                    triangle.a.coords,
                    triangle.b.coords,
                    triangle.c.coords,
                    Color::opaque(200, 200, 200),
                );
            } else if let Some(capsule) = collider.shape().as_capsule() {
                context.draw_segment_capsule(
                    capsule.segment.a.coords,
                    capsule.segment.b.coords,
                    capsule.radius,
                    10,
                    10,
                    transform,
                    Color::opaque(200, 200, 200),
                );
            } else if let Some(heightfield) = collider.shape().as_heightfield() {
                for triangle in heightfield.triangles() {
                    let a = transform.transform_point(&triangle.a);
                    let b = transform.transform_point(&triangle.b);
                    let c = transform.transform_point(&triangle.c);
                    context.draw_triangle(
                        a.coords,
                        b.coords,
                        c.coords,
                        Color::opaque(200, 200, 200),
                    );
                }
            }
        }
    }

    /// Casts a ray with given options.
    pub(crate) fn cast_ray<S: QueryResultsStorage>(
        &self,
        handle_map: &FxHashMap<ColliderHandle, Handle<Node>>,
        opts: RayCastOptions,
        query_buffer: &mut S,
    ) {
        let mut query = self.query.borrow_mut();

        // TODO: Ideally this must be called once per frame, but it seems to be impossible because
        // a body can be deleted during the consecutive calls of this method which will most
        // likely end up in panic because of invalid handle stored in internal acceleration
        // structure. This could be fixed by delaying deleting of bodies/collider to the end
        // of the frame.
        query.update(&self.islands, &self.bodies, &self.colliders);

        query_buffer.clear();
        let ray = Ray::new(
            opts.ray_origin,
            opts.ray_direction
                .try_normalize(f32::EPSILON)
                .unwrap_or_default(),
        );
        query.intersections_with_ray(
            &self.colliders,
            &ray,
            opts.max_len,
            true,
            opts.groups,
            None, // TODO
            |handle, intersection| {
                query_buffer.push(Intersection {
                    collider: handle_map.get(&handle).cloned().unwrap(),
                    normal: intersection.normal,
                    position: ray.point_at(intersection.toi),
                    feature: intersection.feature.into(),
                    toi: intersection.toi,
                })
            },
        );
        if opts.sort_results {
            query_buffer.sort_intersections_by(|a, b| {
                if a.toi > b.toi {
                    Ordering::Greater
                } else if a.toi < b.toi {
                    Ordering::Less
                } else {
                    Ordering::Equal
                }
            })
        }
    }
}

impl Default for PhysicsWorld {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for PhysicsWorld {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "PhysicsWorld")
    }
}

/// See module docs.
#[derive(Debug)]
pub struct Graph {
    pub physics: PhysicsWorld,
    collider_map: FxHashMap<ColliderHandle, Handle<Node>>,
    root: Handle<Node>,
    pool: Pool<Node>,
    stack: Vec<Handle<Node>>,
}

impl Default for Graph {
    fn default() -> Self {
        Self {
            physics: PhysicsWorld::new(),
            collider_map: Default::default(),
            root: Handle::NONE,
            pool: Pool::new(),
            stack: Vec::new(),
        }
    }
}

/// Sub-graph is a piece of graph that was extracted from a graph. It has ownership
/// over its nodes. It is used to temporarily take ownership of a sub-graph. This could
/// be used if you making a scene editor with a command stack - once you reverted a command,
/// that created a complex nodes hierarchy (for example you loaded a model) you must store
/// all added nodes somewhere to be able put nodes back into graph when user decide to re-do
/// command. Sub-graph allows you to do this without invalidating handles to nodes.
#[derive(Debug)]
pub struct SubGraph {
    /// A root node and its [ticket](/rg3d-core/model/struct.Ticket.html).
    pub root: (Ticket<Node>, Node),

    /// A set of descendant nodes with their tickets.
    pub descendants: Vec<(Ticket<Node>, Node)>,
}

fn remap_handles(old_new_mapping: &FxHashMap<Handle<Node>, Handle<Node>>, dest_graph: &mut Graph) {
    // Iterate over instantiated nodes and remap handles.
    for (_, &new_node_handle) in old_new_mapping.iter() {
        let new_node = &mut dest_graph.pool[new_node_handle];

        match new_node {
            Node::Mesh(mesh) => {
                for surface in mesh.surfaces_mut() {
                    for bone_handle in surface.bones.iter_mut() {
                        if let Some(entry) = old_new_mapping.get(bone_handle) {
                            *bone_handle = *entry;
                        }
                    }
                }
            }
            Node::Collider(collider) => {
                let mut shape_mut = collider.shape_mut();
                match *shape_mut {
                    ColliderShape::Trimesh(ref mut trimesh) => {
                        for source in trimesh.sources.iter_mut() {
                            if let Some(entry) = old_new_mapping.get(&source.0) {
                                source.0 = *entry;
                            }
                        }
                    }
                    ColliderShape::Heightfield(ref mut heightfield) => {
                        if let Some(entry) = old_new_mapping.get(&heightfield.geometry_source.0) {
                            heightfield.geometry_source.0 = *entry;
                        }
                    }
                    _ => (),
                }
            }
            Node::Joint(joint) => {
                if let Some(entry) = old_new_mapping.get(&joint.body1()) {
                    joint.set_body1(*entry);
                }
                if let Some(entry) = old_new_mapping.get(&joint.body2()) {
                    joint.set_body2(*entry);
                }
            }
            _ => {}
        }

        for property in new_node.properties.iter_mut() {
            if let PropertyValue::NodeHandle(ref mut handle) = property.value {
                if let Some(new_handle) = old_new_mapping.get(handle) {
                    *handle = *new_handle;
                }
            }
        }

        // LODs also have handles that must be remapped too.
        if let Some(lod_group) = new_node.lod_group_mut() {
            for level in lod_group.levels.iter_mut() {
                level.objects.retain_mut_ext(|object| {
                    if let Some(entry) = old_new_mapping.get(object) {
                        // Replace to mapped.
                        object.0 = *entry;
                        true
                    } else {
                        // Discard invalid handles.
                        false
                    }
                });
            }
        }
    }
}

fn isometric_local_transform(nodes: &Pool<Node>, node: Handle<Node>) -> Matrix4<f32> {
    let transform = nodes[node].local_transform();
    TransformBuilder::new()
        .with_local_position(**transform.position())
        .with_local_rotation(**transform.rotation())
        .with_pre_rotation(**transform.pre_rotation())
        .with_post_rotation(**transform.post_rotation())
        .build()
        .matrix()
}

fn isometric_global_transform(nodes: &Pool<Node>, node: Handle<Node>) -> Matrix4<f32> {
    let parent = nodes[node].parent();
    if parent.is_some() {
        isometric_global_transform(nodes, parent) * isometric_local_transform(nodes, node)
    } else {
        isometric_local_transform(nodes, node)
    }
}

impl Graph {
    /// Creates new graph instance with single root node.
    pub fn new() -> Self {
        let mut pool = Pool::new();
        let mut root = Node::Base(Default::default());
        root.set_name("__ROOT__");
        let root = pool.spawn(root);
        Self {
            physics: Default::default(),
            stack: Vec::new(),
            root,
            pool,
            collider_map: Default::default(),
        }
    }

    /// Adds new node to the graph. Node will be transferred into implementation-defined
    /// storage and you'll get a handle to the node. Node will be automatically attached
    /// to root node of graph, it is required because graph can contain only one root.
    #[inline]
    pub fn add_node(&mut self, mut node: Node) -> Handle<Node> {
        let children = node.children.clone();
        node.children.clear();
        let handle = self.pool.spawn(node);
        if self.root.is_some() {
            self.link_nodes(handle, self.root);
        }
        for child in children {
            self.link_nodes(child, handle);
        }

        handle
    }

    /// Tries to borrow mutable references to two nodes at the same time by given handles. Will
    /// panic if handles overlaps (points to same node).
    pub fn get_two_mut(&mut self, nodes: (Handle<Node>, Handle<Node>)) -> (&mut Node, &mut Node) {
        self.pool.borrow_two_mut(nodes)
    }

    /// Tries to borrow mutable references to three nodes at the same time by given handles. Will
    /// return Err of handles overlaps (points to same node).
    pub fn get_three_mut(
        &mut self,
        nodes: (Handle<Node>, Handle<Node>, Handle<Node>),
    ) -> (&mut Node, &mut Node, &mut Node) {
        self.pool.borrow_three_mut(nodes)
    }

    /// Tries to borrow mutable references to four nodes at the same time by given handles. Will
    /// panic if handles overlaps (points to same node).
    pub fn get_four_mut(
        &mut self,
        nodes: (Handle<Node>, Handle<Node>, Handle<Node>, Handle<Node>),
    ) -> (&mut Node, &mut Node, &mut Node, &mut Node) {
        self.pool.borrow_four_mut(nodes)
    }

    /// Returns root node of current graph.
    pub fn get_root(&self) -> Handle<Node> {
        self.root
    }

    /// Tries to borrow a node, returns Some(node) if the handle is valid, None - otherwise.
    pub fn try_get(&self, handle: Handle<Node>) -> Option<&Node> {
        self.pool.try_borrow(handle)
    }

    /// Tries to mutably borrow a node, returns Some(node) if the handle is valid, None - otherwise.
    pub fn try_get_mut(&mut self, handle: Handle<Node>) -> Option<&mut Node> {
        self.pool.try_borrow_mut(handle)
    }

    /// Destroys node and its children recursively.
    ///
    /// # Notes
    ///
    /// This method does not remove references to the node in other places like animations,
    /// physics, etc. You should prefer to use [Scene::remove_node](crate::scene::Scene::remove_node) -
    /// it automatically breaks all associations between nodes.
    #[inline]
    pub fn remove_node(&mut self, node_handle: Handle<Node>) {
        self.unlink_internal(node_handle);

        self.stack.clear();
        self.stack.push(node_handle);
        while let Some(handle) = self.stack.pop() {
            for &child in self.pool[handle].children().iter() {
                self.stack.push(child);
            }

            // Remove associated entities.
            let node = self.pool.free(handle);
            match node {
                Node::RigidBody(body) => {
                    self.physics.bodies.remove(
                        body.native.get(),
                        &mut self.physics.islands,
                        &mut self.physics.colliders,
                        &mut self.physics.joints,
                    );
                }
                Node::Collider(collider) => {
                    self.collider_map.remove(&collider.native.get());
                    self.physics.colliders.remove(
                        collider.native.get(),
                        &mut self.physics.islands,
                        &mut self.physics.bodies,
                        true,
                    );
                }
                Node::Joint(joint) => {
                    self.physics.joints.remove(
                        joint.native.get(),
                        &mut self.physics.islands,
                        &mut self.physics.bodies,
                        true,
                    );
                }
                _ => (),
            }
        }
    }

    fn unlink_internal(&mut self, node_handle: Handle<Node>) {
        // Replace parent handle of child
        let parent_handle = std::mem::replace(&mut self.pool[node_handle].parent, Handle::NONE);

        // Remove child from parent's children list
        if parent_handle.is_some() {
            let parent = &mut self.pool[parent_handle];
            if let Some(i) = parent.children().iter().position(|h| *h == node_handle) {
                parent.children.remove(i);
            }
        }

        // Remove native collider when detaching a collider node from rigid body node.
        if let Node::Collider(ref mut collider) = self.pool[node_handle] {
            if self.physics.colliders.get(collider.native.get()).is_some() {
                self.collider_map.remove(&collider.native.get());
                self.physics.colliders.remove(
                    collider.native.get(),
                    &mut self.physics.islands,
                    &mut self.physics.bodies,
                    true,
                );
                collider.native.set(ColliderHandle::invalid());
            }
        }
    }

    /// Links specified child with specified parent.
    #[inline]
    pub fn link_nodes(&mut self, child: Handle<Node>, parent: Handle<Node>) {
        self.unlink_internal(child);
        self.pool[child].parent = parent;
        self.pool[parent].children.push(child);
    }

    /// Unlinks specified node from its parent and attaches it to root graph node.
    #[inline]
    pub fn unlink_node(&mut self, node_handle: Handle<Node>) {
        self.unlink_internal(node_handle);
        self.link_nodes(node_handle, self.root);
        self.pool[node_handle]
            .local_transform_mut()
            .set_position(Vector3::default());
    }

    /// Tries to find a copy of `node_handle` in hierarchy tree starting from `root_handle`.
    pub fn find_copy_of(
        &self,
        root_handle: Handle<Node>,
        node_handle: Handle<Node>,
    ) -> Handle<Node> {
        let root = &self.pool[root_handle];
        if root.original_handle_in_resource() == node_handle {
            return root_handle;
        }

        for child_handle in root.children() {
            let out = self.find_copy_of(*child_handle, node_handle);
            if out.is_some() {
                return out;
            }
        }

        Handle::NONE
    }

    /// Searches node using specified compare closure starting from specified node. If nothing
    /// was found [`Handle::NONE`] is returned.
    pub fn find<C>(&self, root_node: Handle<Node>, cmp: &mut C) -> Handle<Node>
    where
        C: FnMut(&Node) -> bool,
    {
        let root = &self.pool[root_node];
        if cmp(root) {
            root_node
        } else {
            let mut result: Handle<Node> = Handle::NONE;
            for child in root.children() {
                let child_handle = self.find(*child, cmp);
                if !child_handle.is_none() {
                    result = child_handle;
                    break;
                }
            }
            result
        }
    }

    /// Searches node with specified name starting from specified node. If nothing was found,
    /// [`Handle::NONE`] is returned.
    pub fn find_by_name(&self, root_node: Handle<Node>, name: &str) -> Handle<Node> {
        self.find(root_node, &mut |node| node.name() == name)
    }

    /// Searches node with specified name starting from root. If nothing was found, `Handle::NONE`
    /// is returned.
    pub fn find_by_name_from_root(&self, name: &str) -> Handle<Node> {
        self.find_by_name(self.root, name)
    }

    /// Searches node using specified compare closure starting from root. If nothing was found,
    /// `Handle::NONE` is returned.
    pub fn find_from_root<C>(&self, cmp: &mut C) -> Handle<Node>
    where
        C: FnMut(&Node) -> bool,
    {
        self.find(self.root, cmp)
    }

    /// Creates deep copy of node with all children. This is relatively heavy operation!
    /// In case if any error happened it returns `Handle::NONE`. This method can be used
    /// to create exact copy of given node hierarchy. For example you can prepare rocket
    /// model: case of rocket will be mesh, and fire from nozzle will be particle system,
    /// and when you fire from rocket launcher you just need to create a copy of such
    /// "prefab".
    ///
    /// # Notes
    ///
    /// This method does *not* copy any animations! You have to copy them manually. In most
    /// cases it is fine to retarget animation from a resource you want, it will create
    /// animation copy from resource that will work with your nodes hierarchy.
    ///
    /// # Implementation notes
    ///
    /// This method automatically remaps bones for copied surfaces.
    ///
    /// Returns tuple where first element is handle to copy of node, and second element -
    /// old-to-new hash map, which can be used to easily find copy of node by its original.
    ///
    /// Filter allows to exclude some nodes from copied hierarchy. It must return false for
    /// odd nodes. Filtering applied only to descendant nodes.
    pub fn copy_node<F>(
        &self,
        node_handle: Handle<Node>,
        dest_graph: &mut Graph,
        filter: &mut F,
    ) -> (Handle<Node>, FxHashMap<Handle<Node>, Handle<Node>>)
    where
        F: FnMut(Handle<Node>, &Node) -> bool,
    {
        let mut old_new_mapping = FxHashMap::default();
        let root_handle = self.copy_node_raw(node_handle, dest_graph, &mut old_new_mapping, filter);

        remap_handles(&old_new_mapping, dest_graph);

        (root_handle, old_new_mapping)
    }

    /// Creates deep copy of node with all children. This is relatively heavy operation!
    /// In case if any error happened it returns `Handle::NONE`. This method can be used
    /// to create exact copy of given node hierarchy. For example you can prepare rocket
    /// model: case of rocket will be mesh, and fire from nozzle will be particle system,
    /// and when you fire from rocket launcher you just need to create a copy of such
    /// "prefab".
    ///
    /// # Notes
    ///
    /// This method has exactly the same functionality as `copy_node`, but copies not in-place.
    /// This method does *not* copy any animations! You have to copy them manually. In most
    /// cases it is fine to retarget animation from a resource you want, it will create
    /// animation copy from resource that will work with your nodes hierarchy.
    ///
    /// # Implementation notes
    ///
    /// This method automatically remaps bones for copied surfaces.
    ///
    /// Returns tuple where first element is handle to copy of node, and second element -
    /// old-to-new hash map, which can be used to easily find copy of node by its original.
    ///
    /// Filter allows to exclude some nodes from copied hierarchy. It must return false for
    /// odd nodes. Filtering applied only to descendant nodes.
    pub fn copy_node_inplace<F>(
        &mut self,
        node_handle: Handle<Node>,
        filter: &mut F,
    ) -> (Handle<Node>, FxHashMap<Handle<Node>, Handle<Node>>)
    where
        F: FnMut(Handle<Node>, &Node) -> bool,
    {
        let mut old_new_mapping = FxHashMap::default();

        let to_copy = self
            .traverse_handle_iter(node_handle)
            .map(|node| (node, self.pool[node].children.clone()))
            .collect::<Vec<_>>();

        let mut root_handle = Handle::NONE;

        for (parent, children) in to_copy.iter() {
            // Copy parent first.
            let parent_copy = self.pool[*parent].raw_copy();
            let parent_copy_handle = self.add_node(parent_copy);
            old_new_mapping.insert(*parent, parent_copy_handle);

            if root_handle.is_none() {
                root_handle = parent_copy_handle;
            }

            // Copy children and link to new parent.
            for &child in children {
                if filter(child, &self.pool[child]) {
                    let child_copy = self.pool[child].raw_copy();
                    let child_copy_handle = self.add_node(child_copy);
                    old_new_mapping.insert(child, child_copy_handle);
                    self.link_nodes(child_copy_handle, parent_copy_handle);
                }
            }
        }

        remap_handles(&old_new_mapping, self);

        (root_handle, old_new_mapping)
    }

    /// Creates copy of a node and breaks all connections with other nodes. Keep in mind that
    /// this method may give unexpected results when the node has connections with other nodes.
    /// For example if you'll try to copy a skinned mesh, its copy won't be skinned anymore -
    /// you'll get just a "shallow" mesh. Also unlike [copy_node](struct.Graph.html#method.copy_node)
    /// this method returns copied node directly, it does not inserts it in any graph.
    pub fn copy_single_node(&self, node_handle: Handle<Node>) -> Node {
        let node = &self.pool[node_handle];
        let mut clone = node.raw_copy();
        clone.parent = Handle::NONE;
        clone.children.clear();
        if let Node::Mesh(ref mut mesh) = clone {
            for surface in mesh.surfaces_mut() {
                surface.bones.clear();
            }
        }
        clone
    }

    fn copy_node_raw<F>(
        &self,
        root_handle: Handle<Node>,
        dest_graph: &mut Graph,
        old_new_mapping: &mut FxHashMap<Handle<Node>, Handle<Node>>,
        filter: &mut F,
    ) -> Handle<Node>
    where
        F: FnMut(Handle<Node>, &Node) -> bool,
    {
        let src_node = &self.pool[root_handle];
        let dest_node = src_node.raw_copy();
        let dest_copy_handle = dest_graph.add_node(dest_node);
        old_new_mapping.insert(root_handle, dest_copy_handle);
        for &src_child_handle in src_node.children() {
            if filter(src_child_handle, &self.pool[src_child_handle]) {
                let dest_child_handle =
                    self.copy_node_raw(src_child_handle, dest_graph, old_new_mapping, filter);
                if !dest_child_handle.is_none() {
                    dest_graph.link_nodes(dest_child_handle, dest_copy_handle);
                }
            }
        }
        dest_copy_handle
    }

    /// Searches root node in given hierarchy starting from given node. This method is used
    /// when you need to find a root node of a model in complex graph.
    fn find_model_root(&self, from: Handle<Node>) -> Handle<Node> {
        let mut model_root_handle = from;
        while model_root_handle.is_some() {
            let model_node = &self.pool[model_root_handle];

            if model_node.parent().is_none() {
                // We have no parent on node, then it must be root.
                return model_root_handle;
            }

            if model_node.is_resource_instance_root() {
                return model_root_handle;
            }

            // Continue searching up on hierarchy.
            model_root_handle = model_node.parent();
        }
        model_root_handle
    }

    /// Casts a ray with given options.
    pub fn cast_ray<S: QueryResultsStorage>(&self, opts: RayCastOptions, query_buffer: &mut S) {
        self.physics
            .cast_ray(&self.collider_map, opts, query_buffer)
    }

    pub(in crate) fn resolve(&mut self) {
        Log::writeln(MessageKind::Information, "Resolving graph...".to_owned());

        self.update_hierarchical_data();

        // Iterate over each node in the graph and resolve original handles. Original handle is a handle
        // to a node in resource from which a node was instantiated from. Also sync templated properties
        // if needed and copy surfaces from originals.
        for node in self.pool.iter_mut() {
            if let Some(model) = node.resource() {
                let model = model.state();
                match *model {
                    ResourceState::Ok(ref data) => {
                        let resource_graph = &data.get_scene().graph;

                        let resource_node = match data.mapping {
                            NodeMapping::UseNames => {
                                // For some models we can resolve it only by names of nodes, but this is not
                                // reliable way of doing this, because some editors allow nodes to have same
                                // names for objects, but here we'll assume that modellers will not create
                                // models with duplicated names and user of the engine reads log messages.
                                resource_graph
                                    .pair_iter()
                                    .find_map(|(handle, resource_node)| {
                                        if resource_node.name() == node.name() {
                                            Some((resource_node, handle))
                                        } else {
                                            None
                                        }
                                    })
                            }
                            NodeMapping::UseHandles => {
                                // Use original handle directly.
                                resource_graph
                                    .pool
                                    .try_borrow(node.original_handle_in_resource)
                                    .map(|resource_node| {
                                        (resource_node, node.original_handle_in_resource)
                                    })
                            }
                        };

                        if let Some((resource_node, original)) = resource_node {
                            node.original_handle_in_resource = original;
                            node.inv_bind_pose_transform = resource_node.inv_bind_pose_transform();

                            // Check if we can sync transform of the nodes with resource.
                            let resource_local_transform = resource_node.local_transform();
                            let mut local_transform = node.local_transform_mut();

                            // Position.
                            if !local_transform.position().is_custom() {
                                local_transform.set_position(**resource_local_transform.position());
                            }

                            // Rotation.
                            if !local_transform.rotation().is_custom() {
                                local_transform.set_rotation(**resource_local_transform.rotation());
                            }

                            // Scale.
                            if !local_transform.scale().is_custom() {
                                local_transform.set_scale(**resource_local_transform.scale());
                            }

                            // Pre-Rotation.
                            if !local_transform.pre_rotation().is_custom() {
                                local_transform
                                    .set_pre_rotation(**resource_local_transform.pre_rotation());
                            }

                            // Post-Rotation.
                            if !local_transform.post_rotation().is_custom() {
                                local_transform
                                    .set_post_rotation(**resource_local_transform.post_rotation());
                            }

                            // Rotation Offset.
                            if !local_transform.rotation_offset().is_custom() {
                                local_transform.set_rotation_offset(
                                    **resource_local_transform.rotation_offset(),
                                );
                            }

                            // Rotation Pivot.
                            if !local_transform.rotation_pivot().is_custom() {
                                local_transform.set_rotation_pivot(
                                    **resource_local_transform.rotation_pivot(),
                                );
                            }

                            // Scaling Offset.
                            if !local_transform.scaling_offset().is_custom() {
                                local_transform.set_scaling_offset(
                                    **resource_local_transform.scaling_offset(),
                                );
                            }

                            // Scaling Pivot.
                            if !local_transform.scaling_pivot().is_custom() {
                                local_transform
                                    .set_scaling_pivot(**resource_local_transform.scaling_pivot());
                            }

                            drop(local_transform);

                            if let (Node::Mesh(mesh), Node::Mesh(resource_mesh)) =
                                (node, resource_node)
                            {
                                mesh.clear_surfaces();
                                for resource_surface in resource_mesh.surfaces() {
                                    mesh.add_surface(resource_surface.clone());
                                }
                            }
                        }
                    }
                    ResourceState::Pending { .. } => {
                        panic!("resources must be awaited before doing resolve!")
                    }
                    _ => {}
                }
            }
        }

        Log::writeln(
            MessageKind::Information,
            "Original handles resolved!".to_owned(),
        );

        Log::writeln(MessageKind::Information, "Checking integrity...".to_owned());

        // Check integrity - if a node was added in resource, it must be also added in the graph.
        // However if a node was deleted in resource, we must leave it the graph because there
        // might be some other nodes that were attached to the one that was deleted in resource or
        // a node might be referenced somewhere in user code.
        let instances = self
            .pool
            .pair_iter()
            .filter_map(|(h, n)| {
                if n.is_resource_instance_root {
                    Some((h, n.resource().unwrap()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let instance_count = instances.len();
        let mut restored_count = 0;

        for (instance, resource) in instances {
            let model = resource.state();
            if let ResourceState::Ok(ref data) = *model {
                let resource_graph = &data.get_scene().graph;

                let original = self.pool[instance].original_handle_in_resource;

                if original.is_none() {
                    let instance = &self.pool[instance];
                    Log::writeln(
                        MessageKind::Warning,
                        format!(
                            "There is an instance of resource {} \
                    but original node {} cannot be found!",
                            data.path.display(),
                            instance.name()
                        ),
                    );

                    continue;
                }

                let mut traverse_stack = vec![original];
                while let Some(resource_node_handle) = traverse_stack.pop() {
                    let resource_node = &resource_graph[resource_node_handle];

                    // Root of the resource is not belongs to resource, it is just a convenient way of
                    // consolidation all descendants under a single node.
                    if resource_node_handle != resource_graph.root
                        && self.find_by_name(instance, resource_node.name()).is_none()
                    {
                        Log::writeln(
                            MessageKind::Warning,
                            format!(
                                "Instance of node {} is missing. Restoring integrity...",
                                resource_node.name()
                            ),
                        );

                        // Instantiate missing node.
                        let (copy, mapping) =
                            resource_graph.copy_node(resource_node_handle, self, &mut |_, _| true);

                        restored_count += mapping.len();

                        let mut stack = vec![copy];
                        while let Some(node_handle) = stack.pop() {
                            let node = &mut self.pool[node_handle];
                            node.resource = Some(resource.clone());
                            stack.extend_from_slice(node.children());
                        }

                        // Link it with existing node.
                        if resource_node.parent().is_some() {
                            let parent = self.find_by_name(
                                instance,
                                resource_graph[resource_node.parent()].name(),
                            );

                            if parent.is_some() {
                                self.link_nodes(copy, parent);
                            } else {
                                // Fail-safe route - link with root of instance.
                                self.link_nodes(copy, instance);
                            }
                        } else {
                            // Fail-safe route - link with root of instance.
                            self.link_nodes(copy, instance);
                        }
                    }

                    traverse_stack.extend_from_slice(resource_node.children());
                }
            }
        }

        Log::writeln(
            MessageKind::Information,
            format!(
                "Integrity restored for {} instances! {} new nodes were added!",
                instance_count, restored_count
            ),
        );

        // Taking second reference to self is safe here because we need it only
        // to iterate over graph and find copy of bone node. We won't modify pool
        // while iterating over it, so it is double safe.
        let graph = unsafe { &*(self as *const Graph) };

        // Remap bones as separate pass. We can't do that while resolving original handles because
        // surfaces usually has more than one bone and it is impossible to remap unresolved nodes.
        // Bones remapping is required stage because we copied surface from resource and
        // bones are mapped to nodes in resource, but we must have them mapped to instantiated
        // nodes on scene. To do that we'll try to find a root for each node, and starting from
        // it we'll find corresponding bone nodes.
        for (node_handle, node) in self.pool.pair_iter_mut() {
            if let Node::Mesh(mesh) = node {
                let root_handle = graph.find_model_root(node_handle);

                // Remap bones
                for surface in mesh.surfaces_mut() {
                    for bone_handle in surface.bones.iter_mut() {
                        let copy = graph.find_copy_of(root_handle, *bone_handle);

                        if copy.is_none() {
                            Log::writeln(
                                MessageKind::Error,
                                format!(
                                    "Unable to find bone with name {} \
                                                 starting from node {} in the graph! Bone handle will be removed!",
                                    graph[*bone_handle].name(),
                                    graph[root_handle].name()
                                ),
                            );
                        }

                        *bone_handle = copy;
                    }

                    // Remove invalid handles to prevent panicking.
                    surface.bones.retain(|h| h.is_some())
                }
            }
        }

        // Update cube maps for sky boxes.
        for node in self.linear_iter_mut() {
            if let Node::Camera(camera) = node {
                if let Some(skybox) = camera.skybox_mut() {
                    skybox.create_cubemap().ok();
                }
            }
        }

        Log::writeln(
            MessageKind::Information,
            "Graph resolved successfully!".to_owned(),
        );
    }

    /// Calculates local and global transform, global visibility for each node in graph.
    /// Normally you not need to call this method directly, it will be called automatically
    /// on each frame. However there is one use case - when you setup complex hierarchy and
    /// need to know global transform of nodes before entering update loop, then you can call
    /// this method.
    pub fn update_hierarchical_data(&mut self) {
        fn update_recursively(graph: &Graph, node_handle: Handle<Node>) {
            let node = &graph.pool[node_handle];

            let (parent_global_transform, parent_visibility) =
                if let Some(parent) = graph.pool.try_borrow(node.parent()) {
                    (parent.global_transform(), parent.global_visibility())
                } else {
                    (Matrix4::identity(), true)
                };

            node.global_transform
                .set(parent_global_transform * node.local_transform().matrix());
            node.global_visibility
                .set(parent_visibility && node.visibility());

            for &child in node.children() {
                update_recursively(graph, child);
            }
        }

        update_recursively(self, self.root);
    }

    /// Checks whether given node handle is valid or not.
    pub fn is_valid_handle(&self, node_handle: Handle<Node>) -> bool {
        self.pool.is_valid_handle(node_handle)
    }

    fn sync_native_physics(&mut self) {
        for (handle, node) in self.pool.pair_iter() {
            match node {
                Node::RigidBody(rigid_body) => {
                    if let Some(native) = self.physics.bodies.get_mut(rigid_body.native.get()) {
                        let native = native;

                        // Sync transform.
                        if rigid_body.transform_modified.get() {
                            // Transform was changed by user, sync native rigid body with node's position.
                            native.set_position(
                                Isometry3 {
                                    rotation: **rigid_body.local_transform().rotation(),
                                    translation: Translation3 {
                                        vector: **rigid_body.local_transform().position(),
                                    },
                                },
                                true,
                            );
                            rigid_body.transform_modified.set(false);
                        }

                        // Sync native rigid body's properties with scene node's in case if they
                        // were changed by user.
                        let mut changes = rigid_body.changes.get();
                        if changes.contains(RigidBodyChanges::BODY_TYPE) {
                            native.set_body_type(rigid_body.body_type.into());
                            changes.remove(RigidBodyChanges::BODY_TYPE);
                        }
                        if changes.contains(RigidBodyChanges::LIN_VEL) {
                            native.set_linvel(rigid_body.lin_vel, true);
                            changes.remove(RigidBodyChanges::LIN_VEL);
                        }
                        if changes.contains(RigidBodyChanges::ANG_VEL) {
                            native.set_angvel(rigid_body.ang_vel, true);
                            changes.remove(RigidBodyChanges::ANG_VEL);
                        }
                        if changes.contains(RigidBodyChanges::MASS) {
                            let mut props = *native.mass_properties();
                            props.set_mass(rigid_body.mass, true);
                            native.set_mass_properties(props, true);
                            changes.remove(RigidBodyChanges::MASS);
                        }
                        if changes.contains(RigidBodyChanges::LIN_DAMPING) {
                            native.set_linear_damping(rigid_body.lin_damping);
                            changes.remove(RigidBodyChanges::LIN_DAMPING);
                        }
                        if changes.contains(RigidBodyChanges::ANG_DAMPING) {
                            native.set_angular_damping(rigid_body.ang_damping);
                            changes.remove(RigidBodyChanges::ANG_DAMPING);
                        }
                        if changes.contains(RigidBodyChanges::ROTATION_LOCKED) {
                            native.restrict_rotations(
                                rigid_body.x_rotation_locked,
                                rigid_body.y_rotation_locked,
                                rigid_body.z_rotation_locked,
                                true,
                            );
                            changes.remove(RigidBodyChanges::ROTATION_LOCKED);
                        }
                        rigid_body.changes.set(changes);
                    } else {
                        let mut builder = RigidBodyBuilder::new(rigid_body.body_type.into())
                            .position(Isometry3 {
                                rotation: **rigid_body.local_transform().rotation(),
                                translation: Translation3 {
                                    vector: **rigid_body.local_transform().position(),
                                },
                            })
                            .additional_mass(rigid_body.mass)
                            .angvel(rigid_body.ang_vel)
                            .linvel(rigid_body.lin_vel)
                            .linear_damping(rigid_body.lin_damping)
                            .angular_damping(rigid_body.ang_damping)
                            .restrict_rotations(
                                rigid_body.x_rotation_locked,
                                rigid_body.y_rotation_locked,
                                rigid_body.z_rotation_locked,
                            );

                        if rigid_body.translation_locked {
                            builder = builder.lock_translations();
                        }

                        rigid_body
                            .native
                            .set(self.physics.bodies.insert(builder.build()));

                        Log::writeln(
                            MessageKind::Information,
                            format!(
                                "Native rigid body was created for node {}",
                                rigid_body.name()
                            ),
                        );
                    }
                }
                Node::Collider(collider) => {
                    // The collider node may lack backing native physics collider in case if it
                    // is not attached to a rigid body.
                    if let Some(native) = self.physics.colliders.get_mut(collider.native.get()) {
                        if collider.transform_modified.get() {
                            // Transform was changed by user, sync native rigid body with node's position.
                            native.set_position(Isometry3 {
                                rotation: **collider.local_transform().rotation(),
                                translation: Translation3 {
                                    vector: **collider.local_transform().position(),
                                },
                            });
                            collider.transform_modified.set(false);
                        }

                        let mut changes = collider.changes.get();
                        if changes.contains(ColliderChanges::SHAPE) {
                            let inv_global_transform =
                                isometric_global_transform(&self.pool, handle)
                                    .try_inverse()
                                    .unwrap();
                            if let Some(shape) = collider.shape().clone().into_native_shape(
                                inv_global_transform,
                                handle,
                                &self.pool,
                            ) {
                                native.set_shape(shape);
                                changes.remove(ColliderChanges::SHAPE);
                            }
                        }
                        if changes.contains(ColliderChanges::RESTITUTION) {
                            native.set_restitution(collider.restitution());
                            changes.remove(ColliderChanges::RESTITUTION);
                        }
                        if changes.contains(ColliderChanges::COLLISION_GROUPS) {
                            native.set_collision_groups(InteractionGroups::new(
                                collider.collision_groups().memberships,
                                collider.collision_groups().filter,
                            ));
                            changes.remove(ColliderChanges::COLLISION_GROUPS);
                        }
                        if changes.contains(ColliderChanges::SOLVER_GROUPS) {
                            native.set_solver_groups(InteractionGroups::new(
                                collider.solver_groups().memberships,
                                collider.solver_groups().filter,
                            ));
                            changes.remove(ColliderChanges::SOLVER_GROUPS);
                        }
                        if changes.contains(ColliderChanges::FRICTION) {
                            native.set_friction(collider.friction());
                            changes.remove(ColliderChanges::FRICTION);
                        }
                        if changes.contains(ColliderChanges::IS_SENSOR) {
                            native.set_sensor(collider.is_sensor());
                            changes.remove(ColliderChanges::IS_SENSOR);
                        }
                        collider.changes.set(changes);
                        // TODO: Handle RESTITUTION_COMBINE_RULE + FRICTION_COMBINE_RULE
                    } else if let Some(Node::RigidBody(parent_body)) =
                        self.try_get(collider.parent())
                    {
                        if parent_body.native.get() != RigidBodyHandle::invalid() {
                            let inv_global_transform = self
                                .isometric_global_transform(handle)
                                .try_inverse()
                                .unwrap();
                            let rigid_body_native = parent_body.native.get();
                            if let Some(shape) = collider.shape().clone().into_native_shape(
                                inv_global_transform,
                                handle,
                                &self.pool,
                            ) {
                                let mut builder = ColliderBuilder::new(shape)
                                    .position(Isometry3 {
                                        rotation: **collider.local_transform().rotation(),
                                        translation: Translation3 {
                                            vector: **collider.local_transform().position(),
                                        },
                                    })
                                    .friction(collider.friction())
                                    .restitution(collider.restitution())
                                    .collision_groups(InteractionGroups::new(
                                        collider.collision_groups().memberships,
                                        collider.collision_groups().filter,
                                    ))
                                    .solver_groups(InteractionGroups::new(
                                        collider.solver_groups().memberships,
                                        collider.solver_groups().filter,
                                    ))
                                    .sensor(collider.is_sensor());

                                if let Some(density) = collider.density() {
                                    builder = builder.density(density);
                                }

                                let native_handle = self.physics.colliders.insert_with_parent(
                                    builder.build(),
                                    rigid_body_native,
                                    &mut self.physics.bodies,
                                );
                                self.collider_map.insert(native_handle, handle);
                                collider.native.set(native_handle);

                                Log::writeln(
                                    MessageKind::Information,
                                    format!(
                                        "Native collider was created for node {}",
                                        collider.name()
                                    ),
                                );
                            }
                        }
                    }
                }
                Node::Joint(joint) => {
                    if let Some(native) = self.physics.joints.get_mut(joint.native.get()) {
                        let mut changes = joint.changes.get();
                        if changes.contains(JointChanges::PARAMS) {
                            native.params = joint.params().clone().into();
                            changes.remove(JointChanges::PARAMS);
                        }
                        if changes.contains(JointChanges::BODY1) {
                            // TODO
                            changes.remove(JointChanges::BODY1);
                        }
                        if changes.contains(JointChanges::BODY2) {
                            // TODO
                            changes.remove(JointChanges::BODY2);
                        }
                        joint.changes.set(changes);
                    } else {
                        let body1_handle = joint.body1();
                        let body2_handle = joint.body2();
                        let params = joint.params().clone();

                        // A native joint can be created iff both rigid bodies are correctly assigned.
                        if let (Some(Node::RigidBody(body1)), Some(Node::RigidBody(body2))) = (
                            self.pool.try_borrow(body1_handle),
                            self.pool.try_borrow(body2_handle),
                        ) {
                            let native_body1 = body1.native.get();
                            let native_body2 = body2.native.get();

                            let native =
                                self.physics
                                    .joints
                                    .insert(native_body1, native_body2, params);

                            joint.native.set(native);

                            Log::writeln(
                                MessageKind::Information,
                                format!("Native joint was created for node {}", joint.name()),
                            );
                        }
                    }
                }
                _ => (),
            }
        }
    }

    /// Updates nodes in graph using given delta time. There is no need to call it manually.
    pub fn update(&mut self, frame_size: Vector2<f32>, dt: f32) {
        self.sync_native_physics();

        self.physics.update();

        self.update_hierarchical_data();

        for i in 0..self.pool.get_capacity() {
            let handle = self.pool.handle_from_index(i);

            if let Some(node) = self.pool.at_mut(i) {
                let remove = if let Some(lifetime) = node.lifetime.as_mut() {
                    *lifetime -= dt;
                    *lifetime <= 0.0
                } else {
                    false
                };

                if remove {
                    self.remove_node(handle);
                } else {
                    match node {
                        Node::Camera(camera) => {
                            camera.calculate_matrices(frame_size);

                            let old_cache = camera.visibility_cache.invalidate();
                            let mut new_cache = VisibilityCache::from(old_cache);
                            let observer_position = camera.global_position();
                            let z_near = camera.z_near();
                            let z_far = camera.z_far();
                            let frustum =
                                Frustum::from(camera.view_projection_matrix()).unwrap_or_default();
                            new_cache.update(
                                self,
                                observer_position,
                                z_near,
                                z_far,
                                Some(&[&frustum]),
                            );
                            // We have to re-borrow camera again because borrow check cannot proof that
                            // camera reference is still valid after passing `self` to `new_cache.update(...)`
                            // This is ok since there are only few camera per level and there performance
                            // penalty is negligible.
                            self.pool
                                .at_mut(i)
                                .unwrap()
                                .as_camera_mut()
                                .visibility_cache = new_cache;
                        }
                        Node::ParticleSystem(particle_system) => particle_system.update(dt),
                        Node::Terrain(terrain) => terrain.update(),
                        Node::Mesh(_) => self.pool.at(i).unwrap().as_mesh().update(self),
                        // We have to sync rigid body parameters back after each physics step, hopefully there is
                        // not many data that has to be synced.
                        Node::RigidBody(rigid_body) => {
                            if let Some(native) =
                                self.physics.bodies.get_mut(rigid_body.native.get())
                            {
                                rigid_body
                                    .local_transform
                                    .set_position(native.position().translation.vector)
                                    .set_rotation(native.position().rotation);
                                rigid_body.lin_vel = *native.linvel();
                                rigid_body.ang_vel = *native.angvel();
                            }
                        }
                        _ => (),
                    }
                }
            }
        }
    }

    /// Returns capacity of internal pool. Can be used to iterate over all **potentially**
    /// available indices and try to convert them to handles.
    ///
    /// ```
    /// use rg3d::scene::node::Node;
    /// use rg3d::scene::graph::Graph;
    /// let mut graph = Graph::new();
    /// graph.add_node(Node::Base(Default::default()));
    /// graph.add_node(Node::Base(Default::default()));
    /// for i in 0..graph.capacity() {
    ///     let handle = graph.handle_from_index(i);
    ///     if handle.is_some() {
    ///         let node = &mut graph[handle];
    ///         // Do something with node.
    ///     }
    /// }
    /// ```
    pub fn capacity(&self) -> u32 {
        self.pool.get_capacity()
    }

    /// Makes new handle from given index. Handle will be none if index was either out-of-bounds
    /// or point to a vacant pool entry.
    ///
    /// ```
    /// use rg3d::scene::node::Node;
    /// use rg3d::scene::graph::Graph;
    /// let mut graph = Graph::new();
    /// graph.add_node(Node::Base(Default::default()));
    /// graph.add_node(Node::Base(Default::default()));
    /// for i in 0..graph.capacity() {
    ///     let handle = graph.handle_from_index(i);
    ///     if handle.is_some() {
    ///         let node = &mut graph[handle];
    ///         // Do something with node.
    ///     }
    /// }
    /// ```
    pub fn handle_from_index(&self, index: u32) -> Handle<Node> {
        self.pool.handle_from_index(index)
    }

    /// Creates an iterator that has linear iteration order over internal collection
    /// of nodes. It does *not* perform any tree traversal!
    pub fn linear_iter(&self) -> PoolIterator<Node> {
        self.pool.iter()
    }

    /// Creates an iterator that has linear iteration order over internal collection
    /// of nodes. It does *not* perform any tree traversal!
    pub fn linear_iter_mut(&mut self) -> PoolIteratorMut<Node> {
        self.pool.iter_mut()
    }

    /// Creates new iterator that iterates over internal collection giving (handle; node) pairs.
    pub fn pair_iter(&self) -> PoolPairIterator<Node> {
        self.pool.pair_iter()
    }

    /// Creates new iterator that iterates over internal collection giving (handle; node) pairs.
    pub fn pair_iter_mut(&mut self) -> PoolPairIteratorMut<Node> {
        self.pool.pair_iter_mut()
    }

    /// Extracts node from graph and reserves its handle. It is used to temporarily take
    /// ownership over node, and then put node back using given ticket. Extracted node is
    /// detached from its parent!
    pub fn take_reserve(&mut self, handle: Handle<Node>) -> (Ticket<Node>, Node) {
        self.unlink_internal(handle);
        self.pool.take_reserve(handle)
    }

    /// Puts node back by given ticket. Attaches back to root node of graph.
    pub fn put_back(&mut self, ticket: Ticket<Node>, node: Node) -> Handle<Node> {
        let handle = self.pool.put_back(ticket, node);
        self.link_nodes(handle, self.root);
        handle
    }

    /// Makes node handle vacant again.
    pub fn forget_ticket(&mut self, ticket: Ticket<Node>) {
        self.pool.forget_ticket(ticket)
    }

    /// Extracts sub-graph starting from a given node. All handles to extracted nodes
    /// becomes reserved and will be marked as "occupied", an attempt to borrow a node
    /// at such handle will result in panic!. Please note that root node will be
    /// detached from its parent!
    pub fn take_reserve_sub_graph(&mut self, root: Handle<Node>) -> SubGraph {
        // Take out descendants first.
        let mut descendants = Vec::new();
        let mut stack = self[root].children().to_vec();
        while let Some(handle) = stack.pop() {
            stack.extend_from_slice(self[handle].children());
            descendants.push(self.pool.take_reserve(handle));
        }

        SubGraph {
            // Root must be extracted with detachment from its parent (if any).
            root: self.take_reserve(root),
            descendants,
        }
    }

    /// Puts previously extracted sub-graph into graph. Handles to nodes will become valid
    /// again. After that you probably want to re-link returned handle with its previous
    /// parent.
    pub fn put_sub_graph_back(&mut self, sub_graph: SubGraph) -> Handle<Node> {
        for (ticket, node) in sub_graph.descendants {
            self.pool.put_back(ticket, node);
        }

        let (ticket, node) = sub_graph.root;
        let root_handle = self.put_back(ticket, node);

        self.link_nodes(root_handle, self.root);

        root_handle
    }

    /// Forgets the entire sub-graph making handles to nodes invalid.
    pub fn forget_sub_graph(&mut self, sub_graph: SubGraph) {
        for (ticket, _) in sub_graph.descendants {
            self.pool.forget_ticket(ticket);
        }
        let (ticket, _) = sub_graph.root;
        self.pool.forget_ticket(ticket);
    }

    /// Returns the number of nodes in the graph.
    pub fn node_count(&self) -> u32 {
        self.pool.alive_count()
    }

    /// Create a graph depth traversal iterator.
    ///
    /// # Notes
    ///
    /// This method allocates temporal array so it is not cheap! Should not be
    /// used on each frame.
    pub fn traverse_iter(&self, from: Handle<Node>) -> GraphTraverseIterator {
        GraphTraverseIterator {
            graph: self,
            stack: vec![from],
        }
    }

    /// Create a graph depth traversal iterator which will emit *handles* to nodes.
    ///
    /// # Notes
    ///
    /// This method allocates temporal array so it is not cheap! Should not be
    /// used on each frame.
    pub fn traverse_handle_iter(&self, from: Handle<Node>) -> GraphHandleTraverseIterator {
        GraphHandleTraverseIterator {
            graph: self,
            stack: vec![from],
        }
    }

    /// Creates deep copy of graph. Allows filtering while copying, returns copy and
    /// old-to-new node mapping.
    pub fn clone<F>(&self, filter: &mut F) -> (Self, FxHashMap<Handle<Node>, Handle<Node>>)
    where
        F: FnMut(Handle<Node>, &Node) -> bool,
    {
        let mut copy = Self::default();
        let (root, old_new_map) = self.copy_node(self.root, &mut copy, filter);
        copy.root = root;
        (copy, old_new_map)
    }

    /// Returns local transformation matrix of a node without scale.
    pub fn local_transform_no_scale(&self, node: Handle<Node>) -> Matrix4<f32> {
        let mut transform = self[node].local_transform().clone();
        transform.set_scale(Vector3::new(1.0, 1.0, 1.0));
        transform.matrix()
    }

    /// Returns world transformation matrix of a node without scale.
    pub fn global_transform_no_scale(&self, node: Handle<Node>) -> Matrix4<f32> {
        let parent = self[node].parent();
        if parent.is_some() {
            self.global_transform_no_scale(parent) * self.local_transform_no_scale(node)
        } else {
            self.local_transform_no_scale(node)
        }
    }

    /// Returns isometric local transformation matrix of a node. Such transform has
    /// only translation and rotation.
    pub fn isometric_local_transform(&self, node: Handle<Node>) -> Matrix4<f32> {
        isometric_local_transform(&self.pool, node)
    }

    /// Returns world transformation matrix of a node only.  Such transform has
    /// only translation and rotation.
    pub fn isometric_global_transform(&self, node: Handle<Node>) -> Matrix4<f32> {
        isometric_global_transform(&self.pool, node)
    }

    /// Returns global scale matrix of a node.
    pub fn global_scale_matrix(&self, node: Handle<Node>) -> Matrix4<f32> {
        let node = &self[node];
        let local_scale_matrix = Matrix4::new_nonuniform_scaling(node.local_transform().scale());
        if node.parent().is_some() {
            self.global_scale_matrix(node.parent()) * local_scale_matrix
        } else {
            local_scale_matrix
        }
    }

    /// Returns rotation quaternion of a node in world coordinates.
    pub fn global_rotation(&self, node: Handle<Node>) -> UnitQuaternion<f32> {
        UnitQuaternion::from(Rotation3::from_matrix(
            &self.global_transform_no_scale(node).basis(),
        ))
    }

    /// Returns rotation quaternion of a node in world coordinates without pre- and post-rotations.
    pub fn isometric_global_rotation(&self, node: Handle<Node>) -> UnitQuaternion<f32> {
        UnitQuaternion::from(Rotation3::from_matrix(
            &self.isometric_global_transform(node).basis(),
        ))
    }

    /// Returns rotation quaternion and position of a node in world coordinates, scale is eliminated.
    pub fn global_rotation_position_no_scale(
        &self,
        node: Handle<Node>,
    ) -> (UnitQuaternion<f32>, Vector3<f32>) {
        (self.global_rotation(node), self[node].global_position())
    }

    /// Returns isometric global rotation and position.
    pub fn isometric_global_rotation_position(
        &self,
        node: Handle<Node>,
    ) -> (UnitQuaternion<f32>, Vector3<f32>) {
        (
            self.isometric_global_rotation(node),
            self[node].global_position(),
        )
    }

    /// Returns global scale of a node.
    pub fn global_scale(&self, node: Handle<Node>) -> Vector3<f32> {
        let m = self.global_scale_matrix(node);
        Vector3::new(m[0], m[5], m[10])
    }
}

impl Index<Handle<Node>> for Graph {
    type Output = Node;

    fn index(&self, index: Handle<Node>) -> &Self::Output {
        &self.pool[index]
    }
}

impl IndexMut<Handle<Node>> for Graph {
    fn index_mut(&mut self, index: Handle<Node>) -> &mut Self::Output {
        &mut self.pool[index]
    }
}

/// Iterator that traverses tree in depth and returns shared references to nodes.
pub struct GraphTraverseIterator<'a> {
    graph: &'a Graph,
    stack: Vec<Handle<Node>>,
}

impl<'a> Iterator for GraphTraverseIterator<'a> {
    type Item = &'a Node;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(handle) = self.stack.pop() {
            let node = &self.graph[handle];

            for child_handle in node.children() {
                self.stack.push(*child_handle);
            }

            return Some(node);
        }

        None
    }
}

/// Iterator that traverses tree in depth and returns handles to nodes.
pub struct GraphHandleTraverseIterator<'a> {
    graph: &'a Graph,
    stack: Vec<Handle<Node>>,
}

impl<'a> Iterator for GraphHandleTraverseIterator<'a> {
    type Item = Handle<Node>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(handle) = self.stack.pop() {
            for child_handle in self.graph[handle].children() {
                self.stack.push(*child_handle);
            }

            return Some(handle);
        }
        None
    }
}

impl Visit for Graph {
    fn visit(&mut self, name: &str, visitor: &mut Visitor) -> VisitResult {
        visitor.enter_region(name)?;

        // Pool must be empty, otherwise handles will be invalid and everything will blow up.
        if visitor.is_reading() && self.pool.get_capacity() != 0 {
            panic!("Graph pool must be empty on load!")
        }

        self.root.visit("Root", visitor)?;
        self.pool.visit("Pool", visitor)?;
        // self.physics is not serialized intentionally! The data of physics entities stored
        // inside graph nodes and corresponding physic entities will be re-created on first
        // update iteration.

        visitor.leave_region()
    }
}

#[cfg(test)]
mod test {
    use crate::{
        core::pool::Handle,
        scene::{base::Base, graph::Graph, node::Node},
    };

    #[test]
    fn graph_init_test() {
        let graph = Graph::new();
        assert_ne!(graph.root, Handle::NONE);
        assert_eq!(graph.pool.alive_count(), 1);
    }

    #[test]
    fn graph_node_test() {
        let mut graph = Graph::new();
        graph.add_node(Node::Base(Base::default()));
        graph.add_node(Node::Base(Base::default()));
        graph.add_node(Node::Base(Base::default()));
        assert_eq!(graph.pool.alive_count(), 4);
    }
}
