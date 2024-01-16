from typing import Any, Dict, Type, TypeVar, Tuple, Optional, BinaryIO, TextIO, TYPE_CHECKING

from typing import List


from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset







T = TypeVar("T", bound="ListQueueResponse200ItemFlowStatusModulesItemBranchall")


@_attrs_define
class ListQueueResponse200ItemFlowStatusModulesItemBranchall:
    """ 
        Attributes:
            branch (int):
            len_ (int):
     """

    branch: int
    len_: int
    additional_properties: Dict[str, Any] = _attrs_field(init=False, factory=dict)


    def to_dict(self) -> Dict[str, Any]:
        branch = self.branch
        len_ = self.len_

        field_dict: Dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update({
            "branch": branch,
            "len": len_,
        })

        return field_dict



    @classmethod
    def from_dict(cls: Type[T], src_dict: Dict[str, Any]) -> T:
        d = src_dict.copy()
        branch = d.pop("branch")

        len_ = d.pop("len")

        list_queue_response_200_item_flow_status_modules_item_branchall = cls(
            branch=branch,
            len_=len_,
        )

        list_queue_response_200_item_flow_status_modules_item_branchall.additional_properties = d
        return list_queue_response_200_item_flow_status_modules_item_branchall

    @property
    def additional_keys(self) -> List[str]:
        return list(self.additional_properties.keys())

    def __getitem__(self, key: str) -> Any:
        return self.additional_properties[key]

    def __setitem__(self, key: str, value: Any) -> None:
        self.additional_properties[key] = value

    def __delitem__(self, key: str) -> None:
        del self.additional_properties[key]

    def __contains__(self, key: str) -> bool:
        return key in self.additional_properties