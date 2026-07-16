import type { Decorator } from "@storybook/react-vite"
import { MemoryRouter } from "react-router"
import { AppSidebar } from "@/components/app-sidebar"
import { SidebarProvider } from "@/components/ui/sidebar"

export const withSettingsFrame: Decorator = (Story, context) => {
  const route =
    typeof context.parameters.route === "string"
      ? context.parameters.route
      : "/"

  return (
    <MemoryRouter initialEntries={[route]}>
      <SidebarProvider>
        <AppSidebar />
        <main className="w-full p-6">
          <Story />
        </main>
      </SidebarProvider>
    </MemoryRouter>
  )
}
