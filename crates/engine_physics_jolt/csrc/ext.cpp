// What JoltC does not expose and the engine needs (ADR 0066), written against Jolt's C++
// API: character controllers (CharacterVirtual), state save/restore into a caller's
// buffer (StateRecorder), and a counter of Jolt's heap allocations for zero-allocation
// tests. Written for this engine; not part of Jolt or JoltC. Plain C structs cross the
// boundary; their sizes are checked by the Rust layout test (src/ffi.rs).

#include "joltc.h"

#include <Jolt/Jolt.h>
#include <Jolt/Core/Memory.h>
#include <Jolt/Core/TempAllocator.h>
#include <Jolt/Physics/Body/BodyInterface.h>
#include <Jolt/Physics/Character/CharacterVirtual.h>
#include <Jolt/Physics/Collision/Shape/CapsuleShape.h>
#include <Jolt/Physics/Collision/Shape/RotatedTranslatedShape.h>
#include <Jolt/Physics/PhysicsSystem.h>
#include <Jolt/Physics/StateRecorder.h>

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <cstring>

// JoltC's handle for a physics system (joltc.cpp keeps the definition private). The same
// members in the same order; `engine_jolt_world_create` checks the layout at run time by
// comparing the body interface reached through it with the one JoltC returns.
struct JPH_PhysicsSystem final
{
	JPH::BroadPhaseLayerInterface* broadPhaseLayerInterface = nullptr;
	JPH::ObjectLayerPairFilter* objectLayerPairFilter = nullptr;
	JPH::ObjectVsBroadPhaseLayerFilter* objectVsBroadPhaseLayerFilter = nullptr;
	JPH::PhysicsSystem* physicsSystem = nullptr;
};

using namespace JPH;

// ---------------------------------------------------------------------------------------
// Allocation counter: wraps Jolt's allocation hooks (installed by JPH_Init) once.

static std::atomic<uint64_t> s_allocations{0};
static AllocateFunction s_allocate = nullptr;
static ReallocateFunction s_reallocate = nullptr;
static AlignedAllocateFunction s_aligned_allocate = nullptr;

static void* CountedAllocate(size_t size)
{
	s_allocations.fetch_add(1, std::memory_order_relaxed);
	return s_allocate(size);
}

static void* CountedReallocate(void* block, size_t old_size, size_t new_size)
{
	s_allocations.fetch_add(1, std::memory_order_relaxed);
	return s_reallocate(block, old_size, new_size);
}

static void* CountedAlignedAllocate(size_t size, size_t alignment)
{
	s_allocations.fetch_add(1, std::memory_order_relaxed);
	return s_aligned_allocate(size, alignment);
}

// Call once, after JPH_Init and before any other thread uses Jolt.
extern "C" void engine_jolt_count_allocations(void)
{
	if (s_allocate != nullptr)
		return;
	s_allocate = Allocate;
	s_reallocate = Reallocate;
	s_aligned_allocate = AlignedAllocate;
	Allocate = CountedAllocate;
	Reallocate = CountedReallocate;
	AlignedAllocate = CountedAlignedAllocate;
}

// Heap allocations Jolt made so far (all threads, all worlds).
extern "C" uint64_t engine_jolt_allocations(void)
{
	return s_allocations.load(std::memory_order_relaxed);
}

// ---------------------------------------------------------------------------------------
// Per-world state.

struct EngineJoltWorld
{
	PhysicsSystem* system;
	// Temporary memory for character updates; this world's own, so character moves need
	// no lock against other worlds (JoltC's shared temporary allocator does).
	TempAllocatorImplWithMallocFallback temp{1024 * 1024};
	CharacterVsCharacterCollisionSimple characters;
};

extern "C" EngineJoltWorld* engine_jolt_world_create(JPH_PhysicsSystem* system)
{
	if (system == nullptr || system->physicsSystem == nullptr)
		return nullptr;
	// The layout check: JoltC's accessor must agree with our view of its struct.
	if (reinterpret_cast<BodyInterface*>(JPH_PhysicsSystem_GetBodyInterface(system)) != &system->physicsSystem->GetBodyInterface())
		return nullptr;
	EngineJoltWorld* world = new EngineJoltWorld();
	world->system = system->physicsSystem;
	return world;
}

extern "C" void engine_jolt_world_destroy(EngineJoltWorld* world)
{
	delete world;
}

// ---------------------------------------------------------------------------------------
// Characters.

struct EngineJoltCharacterDesc
{
	float position[3];
	float rotation[4];
	float up[3];
	float half_height; // of the cylinder part
	float radius;
	float max_slope; // radians
	float step_height;
	float snap_to_ground;
	float mass;
	float max_push_force;
	uint32_t id;           // the character id (deterministic per world)
	uint32_t object_layer; // what it collides with
	uint32_t proxy_layer;  // object layer of the proxy body, or UINT32_MAX for none
	uint32_t _padding;
	uint64_t user_data;
};

struct EngineJoltCharacterState
{
	float position[3];
	float rotation[4];
	float velocity[3];
	float ground_normal[3];
	float ground_point[3];
	float ground_velocity[3];
	uint32_t ground_state; // EGroundState
	uint32_t ground_body;  // JPH_BodyID, or 0xFFFFFFFF
	uint32_t proxy_body;   // JPH_BodyID, or 0xFFFFFFFF
	uint32_t _padding;
};

struct EngineJoltCharacterContact
{
	float point[3];
	float normal[3];
	uint32_t body;      // JPH_BodyID, or 0xFFFFFFFF
	uint32_t character; // character id, or 0xFFFFFFFF
	uint32_t collided;
	uint32_t sensor;
};

struct EngineJoltCharacter
{
	Ref<CharacterVirtual> character;
	Vec3 up;
	float step_height;
	float snap_to_ground;
	ObjectLayer layer;
};

static void Write3(float* out, Vec3Arg v)
{
	out[0] = v.GetX();
	out[1] = v.GetY();
	out[2] = v.GetZ();
}

extern "C" EngineJoltCharacter* engine_jolt_character_create(EngineJoltWorld* world, const EngineJoltCharacterDesc* desc)
{
	Vec3 up(desc->up[0], desc->up[1], desc->up[2]);
	up = up.NormalizedOr(Vec3::sAxisY());
	float half_height = desc->half_height > 0.0f ? desc->half_height : 0.0f;
	float radius = desc->radius;
	// The capsule stands on the character's position (the feet); Jolt's capsules are
	// along Y, so the shape is rotated onto `up`.
	Quat to_up = Quat::sFromTo(Vec3::sAxisY(), up);
	RefConst<Shape> capsule = new RotatedTranslatedShape(up * (half_height + radius), to_up, new CapsuleShape(half_height, radius));

	Ref<CharacterVirtualSettings> settings = new CharacterVirtualSettings();
	settings->mID = CharacterID(desc->id);
	settings->mShape = capsule;
	settings->mUp = up;
	settings->mMaxSlopeAngle = desc->max_slope;
	// Only contacts in the lower hemisphere support the character (Jolt's sample).
	settings->mSupportingVolume = Plane(up, -radius);
	settings->mMass = desc->mass;
	settings->mMaxStrength = desc->max_push_force;
	settings->mEnhancedInternalEdgeRemoval = true;
	if (desc->proxy_layer != UINT32_MAX)
	{
		settings->mInnerBodyShape = capsule;
		settings->mInnerBodyLayer = desc->proxy_layer;
	}

	RVec3 position(desc->position[0], desc->position[1], desc->position[2]);
	Quat rotation(desc->rotation[0], desc->rotation[1], desc->rotation[2], desc->rotation[3]);
	CharacterVirtual* character = new CharacterVirtual(settings, position, rotation.Normalized(), desc->user_data, world->system);
	character->SetCharacterVsCharacterCollision(&world->characters);
	world->characters.Add(character);

	EngineJoltCharacter* handle = new EngineJoltCharacter();
	handle->character = character;
	handle->up = up;
	handle->step_height = desc->step_height;
	handle->snap_to_ground = desc->snap_to_ground;
	handle->layer = desc->object_layer;

	// Find the initial contacts (ground state) at the spawn position.
	PhysicsSystem* system = world->system;
	DefaultBroadPhaseLayerFilter bp = system->GetDefaultBroadPhaseLayerFilter(handle->layer);
	DefaultObjectLayerFilter ol = system->GetDefaultLayerFilter(handle->layer);
	character->RefreshContacts(bp, ol, BodyFilter(), ShapeFilter(), world->temp);
	return handle;
}

extern "C" void engine_jolt_character_destroy(EngineJoltWorld* world, EngineJoltCharacter* handle)
{
	world->characters.Remove(handle->character);
	// Releases the character, which removes and destroys its proxy body.
	delete handle;
}

extern "C" void engine_jolt_character_state(const EngineJoltCharacter* handle, EngineJoltCharacterState* out)
{
	const CharacterVirtual* c = handle->character.GetPtr();
	Write3(out->position, Vec3(c->GetPosition()));
	Quat r = c->GetRotation();
	out->rotation[0] = r.GetX();
	out->rotation[1] = r.GetY();
	out->rotation[2] = r.GetZ();
	out->rotation[3] = r.GetW();
	Write3(out->velocity, c->GetLinearVelocity());
	CharacterBase::EGroundState ground = c->GetGroundState();
	out->ground_state = static_cast<uint32_t>(ground);
	bool touching = ground != CharacterBase::EGroundState::InAir;
	Write3(out->ground_normal, touching ? c->GetGroundNormal() : Vec3::sZero());
	Write3(out->ground_point, touching ? Vec3(c->GetGroundPosition()) : Vec3::sZero());
	Write3(out->ground_velocity, touching ? c->GetGroundVelocity() : Vec3::sZero());
	BodyID body = c->GetGroundBodyID();
	out->ground_body = touching && !body.IsInvalid() ? body.GetIndexAndSequenceNumber() : 0xFFFFFFFFu;
	BodyID proxy = c->GetInnerBodyID();
	out->proxy_body = proxy.IsInvalid() ? 0xFFFFFFFFu : proxy.GetIndexAndSequenceNumber();
	out->_padding = 0;
}

// Moves the character with `velocity` for `dt` (Jolt's ExtendedUpdate: collide and slide,
// slope limit, stairs, stick to floor, pushing) and writes the new state.
extern "C" void engine_jolt_character_move(EngineJoltWorld* world, EngineJoltCharacter* handle, const float* velocity, float dt, EngineJoltCharacterState* out)
{
	CharacterVirtual* c = handle->character;
	c->SetLinearVelocity(Vec3(velocity[0], velocity[1], velocity[2]));
	if (dt > 0.0f)
	{
		CharacterVirtual::ExtendedUpdateSettings settings;
		settings.mStickToFloorStepDown = -handle->up * handle->snap_to_ground;
		settings.mWalkStairsStepUp = handle->up * handle->step_height;
		PhysicsSystem* system = world->system;
		DefaultBroadPhaseLayerFilter bp = system->GetDefaultBroadPhaseLayerFilter(handle->layer);
		DefaultObjectLayerFilter ol = system->GetDefaultLayerFilter(handle->layer);
		c->ExtendedUpdate(dt, system->GetGravity(), settings, bp, ol, BodyFilter(), ShapeFilter(), world->temp);
	}
	engine_jolt_character_state(handle, out);
}

// After a world step: the ground under the character moved; re-read its velocity.
extern "C" void engine_jolt_character_update_ground_velocity(EngineJoltCharacter* handle)
{
	handle->character->UpdateGroundVelocity();
}

extern "C" void engine_jolt_character_set_transform(EngineJoltWorld* world, EngineJoltCharacter* handle, const float* position, const float* rotation)
{
	CharacterVirtual* c = handle->character;
	c->SetPosition(RVec3(position[0], position[1], position[2]));
	c->SetRotation(Quat(rotation[0], rotation[1], rotation[2], rotation[3]).Normalized());
	c->SetLinearVelocity(Vec3::sZero());
	PhysicsSystem* system = world->system;
	DefaultBroadPhaseLayerFilter bp = system->GetDefaultBroadPhaseLayerFilter(handle->layer);
	DefaultObjectLayerFilter ol = system->GetDefaultLayerFilter(handle->layer);
	c->RefreshContacts(bp, ol, BodyFilter(), ShapeFilter(), world->temp);
}

// Writes up to `capacity` contacts of the last move; returns how many there are.
extern "C" uint32_t engine_jolt_character_contacts(const EngineJoltCharacter* handle, EngineJoltCharacterContact* out, uint32_t capacity)
{
	const CharacterVirtual::ContactList& contacts = handle->character->GetActiveContacts();
	uint32_t count = 0;
	for (const CharacterContact& c : contacts)
	{
		if (c.mWasDiscarded && !c.mIsSensorB)
			continue;
		if (count < capacity)
		{
			EngineJoltCharacterContact& o = out[count];
			Write3(o.point, Vec3(c.mPosition));
			Write3(o.normal, c.mContactNormal);
			o.body = c.mBodyB.IsInvalid() ? 0xFFFFFFFFu : c.mBodyB.GetIndexAndSequenceNumber();
			o.character = c.mCharacterIDB.IsInvalid() ? 0xFFFFFFFFu : c.mCharacterIDB.GetValue();
			o.collided = c.mHadCollision ? 1 : 0;
			o.sensor = c.mIsSensorB ? 1 : 0;
		}
		++count;
	}
	return count;
}

// ---------------------------------------------------------------------------------------
// State save / restore into the caller's memory.

class BufferRecorder final : public StateRecorder
{
public:
	BufferRecorder(uint8_t* write, const uint8_t* read, size_t capacity) : mWrite(write), mRead(read), mCapacity(capacity) {}

	void WriteBytes(const void* data, size_t count) override
	{
		// Past the end only the size is counted: the caller grows the buffer and saves
		// again.
		if (mWrite != nullptr && mPosition + count <= mCapacity)
			std::memcpy(mWrite + mPosition, data, count);
		mPosition += count;
	}

	void ReadBytes(void* data, size_t count) override
	{
		if (mRead == nullptr || mPosition + count > mCapacity)
		{
			std::memset(data, 0, count);
			mFailed = true;
			return;
		}
		std::memcpy(data, mRead + mPosition, count);
		mPosition += count;
	}

	bool IsEOF() const override { return mPosition >= mCapacity; }
	bool IsFailed() const override { return mFailed; }

	size_t Position() const { return mPosition; }

private:
	uint8_t* mWrite;
	const uint8_t* mRead;
	size_t mCapacity;
	size_t mPosition = 0;
	bool mFailed = false;
};

// Saves the system and the characters. Returns the size of the state; the bytes are in
// `out` only if it is at most `capacity` (otherwise grow and call again).
extern "C" size_t engine_jolt_save_state(EngineJoltWorld* world, EngineJoltCharacter* const* characters, uint32_t count, uint8_t* out, size_t capacity)
{
	BufferRecorder recorder(out, nullptr, capacity);
	world->system->SaveState(recorder, EStateRecorderState::All);
	recorder.Write(count);
	for (uint32_t i = 0; i < count; ++i)
		characters[i]->character->SaveState(recorder);
	return recorder.Position();
}

// Restores what `engine_jolt_save_state` wrote, for the same characters in the same
// order. Returns false if the bytes do not fit the world.
extern "C" bool engine_jolt_restore_state(EngineJoltWorld* world, EngineJoltCharacter* const* characters, uint32_t count, const uint8_t* bytes, size_t size)
{
	BufferRecorder recorder(nullptr, bytes, size);
	if (!world->system->RestoreState(recorder) || recorder.IsFailed())
		return false;
	uint32_t saved = 0;
	recorder.Read(saved);
	if (recorder.IsFailed() || saved != count)
		return false;
	for (uint32_t i = 0; i < count; ++i)
		characters[i]->character->RestoreState(recorder);
	return !recorder.IsFailed() && recorder.Position() == size;
}

// ---------------------------------------------------------------------------------------
// Struct sizes for the Rust layout test.

extern "C" size_t engine_jolt_sizeof_character_desc(void) { return sizeof(EngineJoltCharacterDesc); }
extern "C" size_t engine_jolt_sizeof_character_state(void) { return sizeof(EngineJoltCharacterState); }
extern "C" size_t engine_jolt_sizeof_character_contact(void) { return sizeof(EngineJoltCharacterContact); }
extern "C" size_t engine_jolt_offsetof_character_desc_user_data(void) { return offsetof(EngineJoltCharacterDesc, user_data); }
extern "C" size_t engine_jolt_offsetof_character_state_ground_state(void) { return offsetof(EngineJoltCharacterState, ground_state); }
extern "C" size_t engine_jolt_sizeof_JPH_ShapeCastSettings(void) { return sizeof(JPH_ShapeCastSettings); }
extern "C" size_t engine_jolt_offsetof_JPH_ShapeCastSettings_returnDeepestPoint(void) { return offsetof(JPH_ShapeCastSettings, returnDeepestPoint); }
extern "C" size_t engine_jolt_sizeof_JPH_ShapeCastResult(void) { return sizeof(JPH_ShapeCastResult); }
extern "C" size_t engine_jolt_offsetof_JPH_ShapeCastResult_fraction(void) { return offsetof(JPH_ShapeCastResult, fraction); }
