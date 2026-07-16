import { Bot, Settings, Megaphone } from "lucide-react"
import { Link, useLocation } from "react-router"

import {
    Sidebar,
    SidebarContent,
    SidebarFooter,
    SidebarGroup,
    SidebarGroupContent,
    SidebarGroupLabel,
    SidebarMenu,
    SidebarMenuButton,
    SidebarMenuItem,
} from "@/components/ui/sidebar"

// Menu items.
const contents = [
    {
        title: "全般",
        url: "/",
        icon: Settings,
    },
    // {
    //     title: "外観",
    //     url: "/appearance",
    //     icon: Palette,
    // },
    {
        title: "Zenzai",
        url: "/zenzai",
        icon: Bot,
    },
]

// Footer items.
const footer = [
    {
        title: "Azookeyについて",
        url: "/about",
        icon: Megaphone,
    },
]

export function AppSidebar() {
    const { pathname } = useLocation()

    return (
        <Sidebar>
            <SidebarContent>
                <SidebarGroup>
                    <SidebarGroupLabel>設定</SidebarGroupLabel>
                    <SidebarGroupContent>
                        <SidebarMenu>
                            {contents.map((item) => (
                                <SidebarMenuItem key={item.title} className={pathname === item.url ? "[&>*]:bg-sidebar-accent" : ""}>
                                    <SidebarMenuButton asChild>
                                        <Link to={item.url}>
                                            <item.icon />
                                            <span>{item.title}</span>
                                        </Link>
                                    </SidebarMenuButton>
                                </SidebarMenuItem>
                            ))}
                        </SidebarMenu>
                    </SidebarGroupContent>
                </SidebarGroup>
            </SidebarContent>
            <SidebarFooter>
                <SidebarGroup>
                    <SidebarGroupContent>
                        <SidebarMenu>
                            {footer.map((item) => (
                                <SidebarMenuItem key={item.title} className={pathname === item.url ? "[&>*]:bg-sidebar-accent" : ""}>
                                    <SidebarMenuButton asChild>
                                        <Link to={item.url}>
                                            <item.icon />
                                            <span>{item.title}</span>
                                        </Link>
                                    </SidebarMenuButton>
                                </SidebarMenuItem>
                            ))}
                        </SidebarMenu>
                    </SidebarGroupContent>
                </SidebarGroup>
            </SidebarFooter>
        </Sidebar>
    )
}
