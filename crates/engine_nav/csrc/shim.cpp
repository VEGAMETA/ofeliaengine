// A C API over Recast and Detour (third_party/recastnavigation, Zlib) for engine_nav
// (ADR 0103). The tile bake follows RecastDemo's Sample_TileMesh::buildTileMesh.

#include "Recast.h"
#include "RecastAlloc.h"
#include "DetourAlloc.h"
#include "DetourNavMesh.h"
#include "DetourNavMeshBuilder.h"
#include "DetourNavMeshQuery.h"

#include <cmath>
#include <cstdint>
#include <cstring>

extern "C" {

// How a tile is baked: world units unless named in cells.
struct EngineNavBake
{
	float cell_size;
	float cell_height;
	float agent_height;
	float agent_radius;
	float agent_max_climb;
	float agent_max_slope;  // degrees
	int region_min_size;    // cells (the area is its square)
	int region_merge_size;  // cells
	float edge_max_len;
	float edge_max_error;   // cells
	int verts_per_poly;
	float detail_sample_dist;      // cells
	float detail_sample_max_error; // cells
	int tile_size;                 // cells
};

// The tile (tx, ty) of a mesh whose tiles start at `origin`, from the triangles that touch
// it: Detour's blob (to free with engine_nav_free) and its size, or null when nothing in it
// is walkable or a step fails (`*status` tells which: 0 empty, 1 out of memory, 2 a step
// failed).
unsigned char* engine_nav_bake_tile(const float* verts, int nverts, const int* tris, int ntris,
	const float* origin, float min_y, float max_y, int tx, int ty, const EngineNavBake* s,
	int* out_size, int* status)
{
	*out_size = 0;
	*status = 0;
	rcConfig cfg;
	std::memset(&cfg, 0, sizeof(cfg));
	cfg.cs = s->cell_size;
	cfg.ch = s->cell_height;
	cfg.walkableSlopeAngle = s->agent_max_slope;
	cfg.walkableHeight = (int)std::ceil(s->agent_height / cfg.ch);
	cfg.walkableClimb = (int)std::floor(s->agent_max_climb / cfg.ch);
	cfg.walkableRadius = (int)std::ceil(s->agent_radius / cfg.cs);
	cfg.maxEdgeLen = (int)(s->edge_max_len / cfg.cs);
	cfg.maxSimplificationError = s->edge_max_error;
	cfg.minRegionArea = s->region_min_size * s->region_min_size;
	cfg.mergeRegionArea = s->region_merge_size * s->region_merge_size;
	cfg.maxVertsPerPoly = s->verts_per_poly;
	cfg.tileSize = s->tile_size;
	cfg.borderSize = cfg.walkableRadius + 3;
	cfg.width = cfg.tileSize + cfg.borderSize * 2;
	cfg.height = cfg.tileSize + cfg.borderSize * 2;
	cfg.detailSampleDist = s->detail_sample_dist < 0.9f ? 0 : cfg.cs * s->detail_sample_dist;
	cfg.detailSampleMaxError = cfg.ch * s->detail_sample_max_error;
	const float tile_world = cfg.tileSize * cfg.cs;
	cfg.bmin[0] = origin[0] + tx * tile_world - cfg.borderSize * cfg.cs;
	cfg.bmin[1] = min_y;
	cfg.bmin[2] = origin[2] + ty * tile_world - cfg.borderSize * cfg.cs;
	cfg.bmax[0] = origin[0] + (tx + 1) * tile_world + cfg.borderSize * cfg.cs;
	cfg.bmax[1] = max_y;
	cfg.bmax[2] = origin[2] + (ty + 1) * tile_world + cfg.borderSize * cfg.cs;

	rcContext ctx(false);
	rcHeightfield* solid = rcAllocHeightfield();
	rcCompactHeightfield* chf = nullptr;
	rcContourSet* cset = nullptr;
	rcPolyMesh* pmesh = nullptr;
	rcPolyMeshDetail* dmesh = nullptr;
	unsigned char* areas = nullptr;
	unsigned char* nav = nullptr;
	int nav_size = 0;

	bool ok = solid && rcCreateHeightfield(&ctx, *solid, cfg.width, cfg.height, cfg.bmin, cfg.bmax, cfg.cs, cfg.ch);
	if (ok && ntris > 0)
	{
		areas = (unsigned char*)rcAlloc(sizeof(unsigned char) * ntris, RC_ALLOC_TEMP);
		ok = areas != nullptr;
		if (ok)
		{
			std::memset(areas, 0, ntris);
			rcMarkWalkableTriangles(&ctx, cfg.walkableSlopeAngle, verts, nverts, tris, ntris, areas);
			ok = rcRasterizeTriangles(&ctx, verts, nverts, tris, areas, ntris, *solid, cfg.walkableClimb);
		}
	}
	if (ok)
	{
		rcFilterLowHangingWalkableObstacles(&ctx, cfg.walkableClimb, *solid);
		rcFilterLedgeSpans(&ctx, cfg.walkableHeight, cfg.walkableClimb, *solid);
		rcFilterWalkableLowHeightSpans(&ctx, cfg.walkableHeight, *solid);
		chf = rcAllocCompactHeightfield();
		ok = chf && rcBuildCompactHeightfield(&ctx, cfg.walkableHeight, cfg.walkableClimb, *solid, *chf);
	}
	rcFreeHeightField(solid);
	solid = nullptr;
	if (ok && chf->spanCount == 0)
	{
		rcFreeCompactHeightfield(chf);
		rcFree(areas);
		return nullptr;
	}
	ok = ok && rcErodeWalkableArea(&ctx, cfg.walkableRadius, *chf)
		&& rcBuildDistanceField(&ctx, *chf)
		&& rcBuildRegions(&ctx, *chf, cfg.borderSize, cfg.minRegionArea, cfg.mergeRegionArea);
	if (ok)
	{
		cset = rcAllocContourSet();
		ok = cset && rcBuildContours(&ctx, *chf, cfg.maxSimplificationError, cfg.maxEdgeLen, *cset);
	}
	if (ok && cset->nconts == 0)
	{
		rcFreeContourSet(cset);
		rcFreeCompactHeightfield(chf);
		rcFree(areas);
		return nullptr;
	}
	if (ok)
	{
		pmesh = rcAllocPolyMesh();
		ok = pmesh && rcBuildPolyMesh(&ctx, *cset, cfg.maxVertsPerPoly, *pmesh);
	}
	if (ok)
	{
		dmesh = rcAllocPolyMeshDetail();
		ok = dmesh && rcBuildPolyMeshDetail(&ctx, *pmesh, *chf, cfg.detailSampleDist, cfg.detailSampleMaxError, *dmesh);
	}
	rcFreeCompactHeightfield(chf);
	rcFreeContourSet(cset);
	rcFree(areas);
	if (ok && pmesh->npolys > 0 && cfg.maxVertsPerPoly <= DT_VERTS_PER_POLYGON)
	{
		// Walkable polygons: area 0, flag 1 (what the default query filter includes).
		for (int i = 0; i < pmesh->npolys; ++i)
		{
			if (pmesh->areas[i] == RC_WALKABLE_AREA)
				pmesh->areas[i] = 0;
			pmesh->flags[i] = 1;
		}
		dtNavMeshCreateParams params;
		std::memset(&params, 0, sizeof(params));
		params.verts = pmesh->verts;
		params.vertCount = pmesh->nverts;
		params.polys = pmesh->polys;
		params.polyAreas = pmesh->areas;
		params.polyFlags = pmesh->flags;
		params.polyCount = pmesh->npolys;
		params.nvp = pmesh->nvp;
		params.detailMeshes = dmesh->meshes;
		params.detailVerts = dmesh->verts;
		params.detailVertsCount = dmesh->nverts;
		params.detailTris = dmesh->tris;
		params.detailTriCount = dmesh->ntris;
		params.walkableHeight = s->agent_height;
		params.walkableRadius = s->agent_radius;
		params.walkableClimb = s->agent_max_climb;
		params.tileX = tx;
		params.tileY = ty;
		params.tileLayer = 0;
		rcVcopy(params.bmin, pmesh->bmin);
		rcVcopy(params.bmax, pmesh->bmax);
		params.cs = cfg.cs;
		params.ch = cfg.ch;
		params.buildBvTree = true;
		ok = dtCreateNavMeshData(&params, &nav, &nav_size);
	}
	else if (ok)
	{
		ok = false;
		*status = 0;
		rcFreePolyMesh(pmesh);
		rcFreePolyMeshDetail(dmesh);
		return nullptr;
	}
	rcFreePolyMesh(pmesh);
	rcFreePolyMeshDetail(dmesh);
	if (!ok)
	{
		*status = 2;
		return nullptr;
	}
	*out_size = nav_size;
	return nav;
}

void engine_nav_free(void* data)
{
	dtFree(data);
}

// A navmesh and a query over it.
struct EngineNavMesh
{
	dtNavMesh* mesh;
	dtNavMeshQuery* query;
	dtQueryFilter filter;
};

EngineNavMesh* engine_nav_create(const float* origin, float tile_width, int max_tiles, int max_polys, int max_nodes)
{
	dtNavMeshParams params;
	std::memset(&params, 0, sizeof(params));
	rcVcopy(params.orig, origin);
	params.tileWidth = tile_width;
	params.tileHeight = tile_width;
	params.maxTiles = max_tiles;
	params.maxPolys = max_polys;
	dtNavMesh* mesh = dtAllocNavMesh();
	if (!mesh || dtStatusFailed(mesh->init(&params)))
	{
		dtFreeNavMesh(mesh);
		return nullptr;
	}
	dtNavMeshQuery* query = dtAllocNavMeshQuery();
	if (!query || dtStatusFailed(query->init(mesh, max_nodes)))
	{
		dtFreeNavMeshQuery(query);
		dtFreeNavMesh(mesh);
		return nullptr;
	}
	EngineNavMesh* nav = new EngineNavMesh();
	nav->mesh = mesh;
	nav->query = query;
	return nav;
}

void engine_nav_destroy(EngineNavMesh* nav)
{
	if (!nav)
		return;
	dtFreeNavMeshQuery(nav->query);
	dtFreeNavMesh(nav->mesh);
	delete nav;
}

// Adds a tile from a copy of `bytes` (Detour writes its links into the tile's data).
// Returns Detour's status.
unsigned int engine_nav_add_tile(EngineNavMesh* nav, const unsigned char* bytes, int size)
{
	unsigned char* data = (unsigned char*)dtAlloc(size, DT_ALLOC_PERM);
	if (!data)
		return DT_FAILURE | DT_OUT_OF_MEMORY;
	std::memcpy(data, bytes, size);
	dtStatus status = nav->mesh->addTile(data, size, DT_TILE_FREE_DATA, 0, nullptr);
	if (dtStatusFailed(status))
		dtFree(data);
	return status;
}

// The nearest point on the mesh to `point`, within `extent` of it. Returns 1 when found.
int engine_nav_nearest(const EngineNavMesh* nav, const float* point, const float* extent, float* out)
{
	dtPolyRef ref = 0;
	float nearest[3];
	if (dtStatusFailed(nav->query->findNearestPoly(point, extent, &nav->filter, &ref, nearest)) || ref == 0)
		return 0;
	rcVcopy(out, nearest);
	return 1;
}

// A path from `start` to `end` (each snapped to the mesh within `extent`): its corners into
// `out` (3 floats each, at most `max_points`), their count in `*count`. Returns 0 when there
// is none, 1 for a complete path, 2 for one to the reachable point nearest `end`.
int engine_nav_path(EngineNavMesh* nav, const float* start, const float* end, const float* extent,
	unsigned long long* polys, int max_polys, float* out, int max_points, int* count)
{
	*count = 0;
	dtPolyRef start_ref = 0, end_ref = 0;
	float a[3], b[3];
	if (dtStatusFailed(nav->query->findNearestPoly(start, extent, &nav->filter, &start_ref, a)) || !start_ref)
		return 0;
	if (dtStatusFailed(nav->query->findNearestPoly(end, extent, &nav->filter, &end_ref, b)) || !end_ref)
		return 0;
	int npolys = 0;
	dtPolyRef* path = (dtPolyRef*)polys;
	dtStatus status = nav->query->findPath(start_ref, end_ref, a, b, &nav->filter, path, &npolys, max_polys);
	if (dtStatusFailed(status) || npolys == 0)
		return 0;
	float target[3];
	rcVcopy(target, b);
	const bool partial = path[npolys - 1] != end_ref || dtStatusDetail(status, DT_PARTIAL_RESULT);
	if (path[npolys - 1] != end_ref)
		nav->query->closestPointOnPoly(path[npolys - 1], b, target, nullptr);
	int n = 0;
	if (dtStatusFailed(nav->query->findStraightPath(a, target, path, npolys, out, nullptr, nullptr, &n, max_points)))
		return 0;
	*count = n;
	return partial ? 2 : 1;
}

// Walks a straight line along the mesh from `start` (snapped within `extent`) towards `end`:
// the fraction of the way it gets before a wall (FLT_MAX when it reaches `end`), or -1 when
// `start` is off the mesh.
float engine_nav_raycast(EngineNavMesh* nav, const float* start, const float* end, const float* extent)
{
	dtPolyRef ref = 0;
	float a[3];
	if (dtStatusFailed(nav->query->findNearestPoly(start, extent, &nav->filter, &ref, a)) || !ref)
		return -1.0f;
	float t = 0.0f;
	float normal[3];
	dtPolyRef visited[64];
	int nvisited = 0;
	if (dtStatusFailed(nav->query->raycast(ref, a, end, &nav->filter, &t, normal, visited, &nvisited, 64)))
		return -1.0f;
	return t;
}

}
