import type { Meta, StoryObj } from "@storybook/react-vite"
import { withSettingsFrame } from "@/storybook/decorators"
import { About } from "./about"

const meta = {
  title: "Pages/About",
  component: About,
  tags: ["autodocs"],
  decorators: [withSettingsFrame],
  parameters: {
    route: "/about",
  },
} satisfies Meta<typeof About>

export default meta
type Story = StoryObj<typeof meta>

export const Default: Story = {}
