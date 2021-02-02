//! Helper functions calling the individual steps of the reconstruction pipeline

use crate::generic_tree::*;
use crate::marching_cubes::SurfacePatch;
use crate::mesh::TriMesh3d;
use crate::octree::{NodeData, Octree, OctreeNode};
use crate::uniform_grid::{OwningSubdomainGrid, Subdomain, UniformGrid};
use crate::workspace::LocalReconstructionWorkspace;
use crate::{
    density_map, marching_cubes, neighborhood_search, new_map, profile, utils, Index, Parameters,
    Real, SpatialDecompositionParameters, SurfaceReconstruction,
};
use log::{debug, info, trace};
use nalgebra::Vector3;

pub(crate) struct SurfaceReconstructionOctreeVisitor<I: Index, R: Real> {
    parameters: Parameters<R>,
    spatial_decomposition: SpatialDecompositionParameters<R>,
    grid: UniformGrid<I, R>,
    octree: Octree<I, R>,
}

impl<I: Index, R: Real> SurfaceReconstructionOctreeVisitor<I, R> {
    pub(crate) fn new(
        global_particle_positions: &[Vector3<R>],
        parameters: &Parameters<R>,
        output_surface: &SurfaceReconstruction<I, R>,
    ) -> Option<Self> {
        // The grid was already generated by the calling public function
        let grid = output_surface.grid.clone();

        // Construct the octree
        let octree = if let Some(decomposition_parameters) = &parameters.spatial_decomposition {
            let margin_factor = decomposition_parameters
                .ghost_particle_safety_factor
                .unwrap_or(R::one());

            Octree::new_subdivided(
                &grid,
                global_particle_positions,
                decomposition_parameters.subdivision_criterion.clone(),
                parameters.compact_support_radius * margin_factor,
                parameters.enable_multi_threading,
                decomposition_parameters.enable_stitching,
            )
        } else {
            // TODO: Use default values instead?

            // If there are no decomposition parameters, we cannot construct an octree.
            return None;
        };

        // Disable all multi-threading in sub-tasks for now (sub-tasks are processed in parallel instead)
        let parameters = {
            let mut p = parameters.clone();
            p.enable_multi_threading = false;
            p
        };

        Some(Self {
            octree,
            spatial_decomposition: parameters.spatial_decomposition.as_ref().unwrap().clone(),
            grid,
            parameters,
        })
    }

    pub(crate) fn run(
        self,
        global_particle_positions: &[Vector3<R>],
        output_surface: &mut SurfaceReconstruction<I, R>,
    ) {
        if self.spatial_decomposition.enable_stitching {
            self.run_with_stitching(global_particle_positions, output_surface);
        } else {
            self.run_inplace(global_particle_positions, output_surface);
        }

        info!(
            "Global mesh has {} triangles and {} vertices.",
            output_surface.mesh.triangles.len(),
            output_surface.mesh.vertices.len()
        );

        output_surface.octree = Some(self.octree);
        output_surface.density_map = None;
    }

    fn run_inplace(
        &self,
        global_particle_positions: &[Vector3<R>],
        output_surface: &mut SurfaceReconstruction<I, R>,
    ) {
        // Clear all local meshes
        {
            let tl_workspaces = &mut output_surface.workspace;
            // Clear all thread local meshes
            tl_workspaces
                .local_workspaces_mut()
                .iter_mut()
                .for_each(|local_workspace| {
                    local_workspace.borrow_mut().mesh.clear();
                });
        }

        // Perform individual surface reconstructions on all non-empty leaves of the octree
        {
            let tl_workspaces = &output_surface.workspace;

            profile!(parent_scope, "parallel domain decomposed surf. rec.");
            info!("Starting triangulation of surface patches.");

            self.octree
                .root()
                .par_visit_bfs(|octree_node: &OctreeNode<I, R>| {
                    let particles = if let Some(particle_set) = octree_node.data().particle_set() {
                        &particle_set.particles
                    } else {
                        // Skip non-leaf nodes
                        return;
                    };

                    profile!("visit octree node for reconstruction", parent = parent_scope);
                    trace!("Processing octree leaf with {} particles", particles.len());

                    if particles.is_empty() {
                        return;
                    } else {
                        let subdomain_grid = self.extract_node_subdomain(octree_node);

                        debug!(
                            "Surface reconstruction of local patch with {} particles. (offset: {:?}, cells_per_dim: {:?})",
                            particles.len(),
                            subdomain_grid.subdomain_offset(),
                            subdomain_grid.subdomain_grid().cells_per_dim());

                        let mut tl_workspace = tl_workspaces
                            .get_local_with_capacity(particles.len())
                            .borrow_mut();

                        // Take particle position storage from workspace and fill it with positions of the leaf
                        let mut node_particle_positions = std::mem::take(&mut tl_workspace.particle_positions);
                        self.collect_node_particle_positions(particles, global_particle_positions, &mut node_particle_positions);

                        // Take the thread local mesh and append without clearinf
                        let mut node_mesh = std::mem::take(&mut tl_workspace.mesh);

                        reconstruct_single_surface_append(
                            &mut *tl_workspace,
                            &self.grid,
                            Some(&subdomain_grid),
                            node_particle_positions.as_slice(),
                            &self.parameters,
                            &mut node_mesh,
                        );

                        trace!("Surface patch successfully processed.");

                        // Put back the particle position and mesh storage
                        tl_workspace.particle_positions = node_particle_positions;
                        tl_workspace.mesh = node_mesh;
                    }
                });
        };

        // Append local meshes to global mesh
        {
            let tl_workspaces = &mut output_surface.workspace;
            // Append all thread local meshes to the global mesh
            tl_workspaces.local_workspaces_mut().iter_mut().fold(
                &mut output_surface.mesh,
                |global_mesh, local_workspace| {
                    global_mesh.append(&mut local_workspace.borrow_mut().mesh);
                    global_mesh
                },
            );
        }
    }

    fn run_with_stitching(
        &self,
        global_particle_positions: &[Vector3<R>],
        output_surface: &mut SurfaceReconstruction<I, R>,
    ) {
        let mut octree = self.octree.clone();

        // Perform individual surface reconstructions on all non-empty leaves of the octree
        {
            let tl_workspaces = &output_surface.workspace;

            profile!(
                parent_scope,
                "parallel domain decomposed surf. rec. with stitching"
            );
            info!("Starting triangulation of surface patches.");

            octree
                .root_mut()
                .par_visit_mut_dfs_post(|octree_node: &mut OctreeNode<I, R>| {
                    profile!("visit octree node (reconstruct or stitch)", parent = parent_scope);

                    let particles = if let Some(particle_set) = octree_node.data().particle_set() {
                        &particle_set.particles
                    } else {
                        // If node has no particle set, its children were already processed so it can be stitched
                        octree_node.stitch_surface_patches(self.parameters.iso_surface_threshold);
                        return;
                    };

                    trace!("Processing octree leaf with {} particles", particles.len());

                    let subdomain_grid = self.extract_node_subdomain(octree_node);
                    let surface_patch = if particles.is_empty() {
                        SurfacePatch::new_empty(subdomain_grid)
                    } else {
                        debug!(
                            "Reconstructing surface of local patch with {} particles. (offset: {:?}, cells_per_dim: {:?})",
                            particles.len(),
                            subdomain_grid.subdomain_offset(),
                            subdomain_grid.subdomain_grid().cells_per_dim()
                        );

                        let mut tl_workspace = tl_workspaces
                            .get_local_with_capacity(particles.len())
                            .borrow_mut();

                        // Take particle position storage from workspace and fill it with positions of the leaf
                        let mut node_particle_positions = std::mem::take(&mut tl_workspace.particle_positions);
                        self.collect_node_particle_positions(particles, global_particle_positions, &mut node_particle_positions);

                        let surface_patch = reconstruct_surface_patch(
                            &mut *tl_workspace,
                            &subdomain_grid,
                            node_particle_positions.as_slice(),
                            &self.parameters,
                        );

                        // Put back the particle position storage
                        tl_workspace.particle_positions = node_particle_positions;

                        surface_patch
                    };

                    trace!("Surface patch successfully processed.");

                    // Store triangulation in the leaf
                    octree_node
                        .data_mut()
                        .replace(NodeData::SurfacePatch(surface_patch.into()));
                });

            info!("Generation of surface patches is done.");
        };

        // Move stitched mesh out of octree
        {
            let surface_path = octree
                .root_mut()
                .data_mut()
                .take()
                .into_surface_patch()
                .expect("Cannot extract stitched mesh from root node")
                .patch;
            output_surface.mesh = surface_path.mesh;
        }
    }

    /// Computes the subdomain grid for the given octree node
    fn extract_node_subdomain(&self, octree_node: &OctreeNode<I, R>) -> OwningSubdomainGrid<I, R> {
        let grid = &self.grid;

        let leaf_aabb = octree_node.aabb(grid);
        let subdomain_grid = octree_node
            .grid(leaf_aabb.min(), grid.cell_size())
            .expect("Unable to construct Octree node grid");
        let subdomain_offset = octree_node.min_corner();
        subdomain_grid.log_grid_info();

        OwningSubdomainGrid::new(grid.clone(), subdomain_grid, *subdomain_offset.index())
    }

    /// Collects the particle positions of all particles in the node
    fn collect_node_particle_positions(
        &self,
        node_particles: &[usize],
        global_particle_positions: &[Vector3<R>],
        node_particle_positions: &mut Vec<Vector3<R>>,
    ) {
        node_particle_positions.clear();
        utils::reserve_total(node_particle_positions, node_particles.len());

        // Extract the particle positions of the leaf
        node_particle_positions.extend(
            node_particles
                .iter()
                .copied()
                .map(|idx| global_particle_positions[idx]),
        );
    }
}

/// Reconstruct a surface, appends triangulation to the given mesh
pub(crate) fn reconstruct_single_surface_append<'a, I: Index, R: Real>(
    workspace: &mut LocalReconstructionWorkspace<I, R>,
    grid: &UniformGrid<I, R>,
    subdomain_grid: Option<&OwningSubdomainGrid<I, R>>,
    particle_positions: &[Vector3<R>],
    parameters: &Parameters<R>,
    output_mesh: &'a mut TriMesh3d<R>,
) {
    let particle_rest_density = parameters.rest_density;
    let particle_rest_volume = R::from_f64((4.0 / 3.0) * std::f64::consts::PI).unwrap()
        * parameters.particle_radius.powi(3);
    let particle_rest_mass = particle_rest_volume * particle_rest_density;

    trace!("Starting neighborhood search...");
    neighborhood_search::search_inplace::<I, R>(
        &grid.aabb(),
        particle_positions,
        parameters.compact_support_radius,
        parameters.enable_multi_threading,
        &mut workspace.particle_neighbor_lists,
    );

    trace!("Computing particle densities...");
    density_map::compute_particle_densities_inplace::<I, R>(
        particle_positions,
        workspace.particle_neighbor_lists.as_slice(),
        parameters.compact_support_radius,
        particle_rest_mass,
        parameters.enable_multi_threading,
        &mut workspace.particle_densities,
    );

    // Create a new density map, reusing memory with the workspace is bad for cache efficiency
    // Alternatively one could reuse memory with a custom caching allocator
    let mut density_map = new_map().into();
    density_map::generate_sparse_density_map(
        grid,
        subdomain_grid,
        particle_positions,
        workspace.particle_densities.as_slice(),
        None,
        particle_rest_mass,
        parameters.compact_support_radius,
        parameters.cube_size,
        parameters.enable_multi_threading,
        &mut density_map,
    );

    marching_cubes::triangulate_density_map_append::<I, R>(
        grid,
        subdomain_grid,
        &density_map,
        parameters.iso_surface_threshold,
        output_mesh,
    );
}

/// Reconstruct a surface, appends triangulation to the given mesh
pub(crate) fn reconstruct_surface_patch<I: Index, R: Real>(
    workspace: &mut LocalReconstructionWorkspace<I, R>,
    subdomain_grid: &OwningSubdomainGrid<I, R>,
    particle_positions: &[Vector3<R>],
    parameters: &Parameters<R>,
) -> SurfacePatch<I, R> {
    profile!("reconstruct_surface_patch");

    let particle_rest_density = parameters.rest_density;
    let particle_rest_volume = R::from_f64((4.0 / 3.0) * std::f64::consts::PI).unwrap()
        * parameters.particle_radius.powi(3);
    let particle_rest_mass = particle_rest_volume * particle_rest_density;

    trace!("Starting neighborhood search...");
    neighborhood_search::search_inplace::<I, R>(
        &subdomain_grid.global_grid().aabb(),
        particle_positions,
        parameters.compact_support_radius,
        parameters.enable_multi_threading,
        &mut workspace.particle_neighbor_lists,
    );

    trace!("Computing particle densities...");
    density_map::compute_particle_densities_inplace::<I, R>(
        particle_positions,
        workspace.particle_neighbor_lists.as_slice(),
        parameters.compact_support_radius,
        particle_rest_mass,
        parameters.enable_multi_threading,
        &mut workspace.particle_densities,
    );

    // Create a new density map, reusing memory with the workspace is bad for cache efficiency
    // Alternatively, one could reuse memory with a custom caching allocator
    let mut density_map = new_map().into();
    density_map::generate_sparse_density_map(
        subdomain_grid.global_grid(),
        Some(subdomain_grid),
        particle_positions,
        workspace.particle_densities.as_slice(),
        None,
        particle_rest_mass,
        parameters.compact_support_radius,
        parameters.cube_size,
        parameters.enable_multi_threading,
        &mut density_map,
    );

    // Run marching cubes and get boundary data
    marching_cubes::triangulate_density_map_to_surface_patch::<I, R>(
        subdomain_grid,
        &density_map,
        parameters.iso_surface_threshold,
    )
}
