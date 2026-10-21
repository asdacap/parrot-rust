// Reassembles an owner's chunked inventory snapshots, with the rules of the terminal CLI's
// QueueSnapshotStreamReader and ShellProcessSnapshotStreamReader: chunks of one instance and revision apply only
// together and in order, an older or repeated revision is ignored, a replaced instance never comes back, and a
// removed inventory stays removed.
export interface InventoryChunk<T> {
  instance: string
  revision: bigint
  index: number
  final: boolean
  removed: boolean
  // What every chunk of one snapshot must agree on, besides instance and revision.
  shape: string
  items: T[]
}

interface Staging<T> {
  removed: boolean
  shape: string
  next: number
  items: T[]
}

export interface OwnerInventory<T> {
  instance: string | undefined
  retired: readonly string[]
  revision: bigint
  observed: boolean
  removed: boolean
  staging: Staging<T> | undefined
  // The last complete snapshot; empty once removed.
  items: T[]
}

export const emptyInventory: OwnerInventory<never> = {
  instance: undefined,
  retired: [],
  revision: 0n,
  observed: false,
  removed: false,
  staging: undefined,
  items: [],
}

export function observeInventory<T>(owner: OwnerInventory<T>, chunk: InventoryChunk<T>): OwnerInventory<T> {
  if (owner.retired.includes(chunk.instance)) return owner
  let current = owner
  if (chunk.index === 0) {
    if (current.instance === chunk.instance) {
      if (current.removed || (current.observed && chunk.revision <= current.revision)) return owner
    } else {
      current = {
        ...current,
        retired: current.instance === undefined ? current.retired : [...current.retired, current.instance],
        instance: chunk.instance,
        removed: false,
      }
    }
    current = {
      ...current,
      revision: chunk.revision,
      observed: true,
      staging: { removed: chunk.removed, shape: chunk.shape, next: 0, items: [] },
    }
  }

  const staging = current.staging
  if (
    staging === undefined ||
    current.instance !== chunk.instance ||
    chunk.revision !== current.revision ||
    chunk.index !== staging.next ||
    chunk.removed !== staging.removed ||
    chunk.shape !== staging.shape
  ) {
    return owner
  }

  const items = [...staging.items, ...chunk.items]
  if (!chunk.final) return { ...current, staging: { ...staging, next: staging.next + 1, items } }
  return { ...current, staging: undefined, removed: chunk.removed, items: chunk.removed ? [] : items }
}
