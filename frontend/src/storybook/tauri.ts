export type ZenzaiConfiguration = {
  zenzai: {
    enable: boolean
    profile: string
    backend: string
  }
}

export type TauriMockState = {
  config: ZenzaiConfiguration
  capabilities: {
    cpu: boolean
    cuda: boolean
    vulkan: boolean
  }
}

const defaultState: TauriMockState = {
  config: {
    zenzai: {
      enable: false,
      profile: "",
      backend: "",
    },
  },
  capabilities: {
    cpu: true,
    cuda: false,
    vulkan: false,
  },
}

let state = clone(defaultState)

function clone<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T
}

export function resetTauriMock() {
  state = clone(defaultState)
}

export function setTauriMock(nextState: Partial<TauriMockState>) {
  state = {
    config: nextState.config ? clone(nextState.config) : state.config,
    capabilities: nextState.capabilities
      ? { ...state.capabilities, ...nextState.capabilities }
      : state.capabilities,
  }
}

export async function invoke<T>(
  command: string,
  args?: { newConfig?: ZenzaiConfiguration },
): Promise<T> {
  switch (command) {
    case "get_config":
      return clone(state.config) as T
    case "check_capability":
      return clone(state.capabilities) as T
    case "update_config":
      if (!args?.newConfig) {
        throw new Error("Storybook mock requires update_config.newConfig")
      }
      state.config = clone(args.newConfig)
      return undefined as T
    default:
      throw new Error(`No Storybook Tauri mock is registered for ${command}`)
  }
}
